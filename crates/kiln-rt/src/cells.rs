//! The two shapes a shard's desk is built from (docs/runtime.md §3.4): a bounded first-in-first-out
//! ring and a bounded stack, both of `Cell`s, both bounded once at the shard's build. Their cells are
//! made as they first hold that many values (kiln's goal 11, plan 01a1a87b2e56: [`Slab`]), so a desk's
//! memory follows the most it has held, not its bound, and the bound stays a bound.
//!
//! A value is moved in by `set` and moved out by `take`: no reference into either structure ever
//! escapes a call, so there is no borrow to check and nothing to refuse but "full" and "empty". They
//! are single-threaded (`Cell` is `!Sync`): only the shard's own thread touches a desk.

use std::cell::Cell;

use crate::slab::Slab;

/// A value alone on its cache line: the largest line the runtime targets, Apple silicon's 128 bytes (64 on
/// the x86_64 and other aarch64 cores it runs on). A word one set of threads writes, kept off a line another
/// thread reads, moves only between the threads that write it.
#[repr(align(128))]
#[derive(Debug, Default)]
pub(crate) struct Padded<T>(pub(crate) T);

/// A bounded first-in-first-out ring of `T`: values are copied in and out, so a pop reads its slot and
/// leaves it as it was, and the indices wrap by a comparison, never a division (a remainder a push and a
/// pop was a hardware divide on the run queue's every listing and taking). A slot is read only between the
/// head and the length, so none needs a mark of its own.
///
/// It wraps at the cells made, not at its capacity: full as far as they go, it makes as many again (at
/// least one, at most up to its capacity) and moves one of its two runs so its values run on in order (the
/// run that wrapped to the start onto the new cells, when they hold it; else the run from the head to the
/// new end), as a growing `VecDeque` does (Rust's `std::collections::VecDeque::grow`). A growth moves fewer
/// values than were made before it, so all the moves of its life come to less than twice its capacity, and
/// the cells made are at most twice the most it has held (kiln's goal 11, plan 01a1a87b2e56).
pub(crate) struct CellRing<T: Copy> {
    slots: Slab<Cell<T>>,
    /// The index of the oldest value.
    head: Cell<usize>,
    /// Values held.
    len: Cell<usize>,
}

impl<T: Copy> std::fmt::Debug for CellRing<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CellRing")
            .field("capacity", &self.slots.capacity())
            .field("made", &self.slots.made())
            .field("len", &self.len.get())
            .finish()
    }
}

impl<T: Copy> CellRing<T> {
    /// A ring that holds at most `capacity` values, none of its cells made.
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            slots: Slab::new(capacity),
            head: Cell::new(0),
            len: Cell::new(0),
        }
    }

    /// The values it can hold.
    pub(crate) fn capacity(&self) -> usize {
        self.slots.capacity()
    }

    /// Values held now.
    pub(crate) fn len(&self) -> usize {
        self.len.get()
    }

    /// Whether it holds none.
    pub(crate) fn is_empty(&self) -> bool {
        self.len.get() == 0
    }

    /// Cells made so far: the most it has held, and at most twice that.
    #[cfg(test)]
    pub(crate) fn made(&self) -> usize {
        self.slots.made()
    }

    /// Appends `value`, or hands it back when the ring is full.
    #[inline]
    pub(crate) fn push(&self, value: T) -> Result<(), T> {
        let len = self.len.get();
        let made = self.slots.made();
        if len >= made {
            return self.grow(value);
        }
        // `head` and `len` are each below the cells made, so their sum is below twice that: one subtraction
        // wraps it.
        let end = self.head.get().wrapping_add(len);
        let at = if end >= made {
            end.wrapping_sub(made)
        } else {
            end
        };
        let Some(slot) = self.slots.get(at) else {
            return Err(value);
        };
        slot.set(value);
        self.len.set(len.wrapping_add(1));
        Ok(())
    }

    /// `push` onto a ring full as far as its cells are made: as many cells again are made (at least one, up
    /// to its capacity), each holding `value` until it is written (a cell is read only between the head and
    /// the length), a run moved so its values run on in order, then `value` goes in. Refused at its capacity.
    #[cold]
    #[inline(never)]
    fn grow(&self, value: T) -> Result<(), T> {
        let made = self.slots.made();
        let room = made
            .max(1)
            .min(self.slots.capacity().saturating_sub(made));
        if room == 0 {
            return Err(value);
        }
        for _ in 0..room {
            if self.slots.make(Cell::new(value)).is_none() {
                return Err(value);
            }
        }
        // Full: its values run from `head` to the old end, then wrap to run from the start up to `head`.
        let head = self.head.get();
        if head != 0 {
            if head <= room {
                // The run at the start goes on past the old end, onto the new cells.
                for at in 0..head {
                    self.copy(at, made.wrapping_add(at));
                }
            } else {
                // The run from `head` goes to the new end, last first (the two places may overlap).
                for at in (head..made).rev() {
                    self.copy(at, at.wrapping_add(room));
                }
                self.head.set(head.wrapping_add(room));
            }
        }
        self.push(value)
    }

    /// Cell `to` takes cell `from`'s value.
    fn copy(&self, from: usize, to: usize) {
        if let (Some(from), Some(to)) = (self.slots.get(from), self.slots.get(to)) {
            to.set(from.get());
        }
    }

    /// Takes the oldest value.
    #[inline]
    pub(crate) fn pop(&self) -> Option<T> {
        let len = self.len.get();
        if len == 0 {
            return None;
        }
        let head = self.head.get();
        let value = self.slots.get(head)?.get();
        // `head` is below the cells made, so the next index is at most that: it wraps by a comparison.
        let next = head.wrapping_add(1);
        self.head
            .set(if next >= self.slots.made() { 0 } else { next });
        self.len.set(len.wrapping_sub(1));
        Some(value)
    }

    /// Each value held, oldest first, without taking any.
    pub(crate) fn each(&self, mut f: impl FnMut(T)) {
        let made = self.slots.made();
        let mut at = self.head.get();
        for _ in 0..self.len.get() {
            if let Some(slot) = self.slots.get(at) {
                f(slot.get());
            }
            at = at.wrapping_add(1);
            if at >= made {
                at = 0;
            }
        }
    }
}

/// A bounded stack of `Copy` values, its cells made as it first holds that many (kiln's goal 11: [`Slab`]).
pub(crate) struct CellStack<T: Copy> {
    slots: Slab<Cell<T>>,
    len: Cell<usize>,
}

impl<T: Copy> std::fmt::Debug for CellStack<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CellStack")
            .field("capacity", &self.slots.capacity())
            .field("made", &self.slots.made())
            .field("len", &self.len.get())
            .finish()
    }
}

impl<T: Copy> CellStack<T> {
    /// An empty stack with room for `capacity` values, none of its cells made (kiln's goal 11: the desk's free
    /// stacks start empty, its slabs making each cell as it is first needed, `crate::slab`).
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            slots: Slab::new(capacity),
            len: Cell::new(0),
        }
    }

    /// A stack holding `values`, the last of them on top.
    pub(crate) fn full_of(values: impl ExactSizeIterator<Item = T>) -> Self {
        let slots = Slab::new(values.len());
        for value in values {
            let _ = slots.make(Cell::new(value));
        }
        let len = slots.made();
        Self {
            slots,
            len: Cell::new(len),
        }
    }

    /// Values held now.
    pub(crate) fn len(&self) -> usize {
        self.len.get()
    }

    /// Takes the most recently pushed value.
    #[inline]
    pub(crate) fn pop(&self) -> Option<T> {
        let len = self.len.get().checked_sub(1)?;
        let value = self.slots.get(len)?.get();
        self.len.set(len);
        Some(value)
    }

    /// Puts `value` back, or hands it back when the stack is full. The cells below the length are all made
    /// (a stack never holds more than it has made), so the one at the length is made, or the next to make.
    #[inline]
    pub(crate) fn push(&self, value: T) -> Result<(), T> {
        let len = self.len.get();
        match self.slots.get(len) {
            Some(slot) => slot.set(value),
            None => {
                if self.slots.make(Cell::new(value)).is_none() {
                    return Err(value);
                }
            }
        }
        self.len.set(len.saturating_add(1));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ring_is_first_in_first_out_and_refuses_past_its_capacity() {
        let ring = CellRing::new(3);
        for value in 0..3 {
            ring.push(value).unwrap();
        }
        assert_eq!(ring.push(9), Err(9));
        assert_eq!(ring.pop(), Some(0));
        ring.push(3).unwrap();
        assert_eq!(
            std::iter::from_fn(|| ring.pop()).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(ring.len.get(), 0);
        assert_eq!(ring.pop(), None);
    }

    #[test]
    fn a_ring_of_no_capacity_refuses_everything() {
        let ring = CellRing::new(0);
        assert_eq!(ring.push(1), Err(1));
        assert_eq!(ring.pop(), None);
    }

    #[test]
    fn a_stack_hands_back_what_was_pushed_last_and_refuses_past_its_capacity() {
        let stack = CellStack::full_of([1u32, 2, 3].into_iter());
        assert_eq!(stack.pop(), Some(3));
        assert_eq!(stack.pop(), Some(2));
        stack.push(7).unwrap();
        assert_eq!(stack.pop(), Some(7));
        assert_eq!(stack.pop(), Some(1));
        assert_eq!(stack.pop(), None);
        for value in 0..3 {
            stack.push(value).unwrap();
        }
        assert_eq!(stack.push(9), Err(9));
    }

    /// kiln's goal 11 (plan 01a1a87b2e56): a ring's cells follow the most it has held, wherever its head is
    /// as it fills (each run moved as it grows), and its values stay first in, first out: against a
    /// `VecDeque` of the same bound, over every head and fill of a small ring, then a long random churn.
    #[test]
    fn g11_a_ring_grows_as_it_fills_and_keeps_its_order() {
        const CAPACITY: usize = 13;
        // Rings filled from heads across them: pushes and pops to move the head there first.
        for warm in 0..CAPACITY {
            for fill in 0..=CAPACITY {
                let ring = CellRing::new(CAPACITY);
                let mut model = std::collections::VecDeque::new();
                let mut next = 0u32;
                for _ in 0..warm {
                    ring.push(next).unwrap();
                    model.push_back(next);
                    next += 1;
                }
                for _ in 0..warm / 2 {
                    assert_eq!(ring.pop(), model.pop_front());
                }
                for _ in 0..fill {
                    let pushed = ring.push(next);
                    if model.len() < CAPACITY {
                        pushed.unwrap();
                        model.push_back(next);
                    } else {
                        assert_eq!(pushed, Err(next), "the bound stays a bound");
                    }
                    next += 1;
                }
                let mut seen = Vec::new();
                ring.each(|value| seen.push(value));
                assert_eq!(seen, model.iter().copied().collect::<Vec<_>>());
                assert!(
                    ring.made() <= (model.len().max(warm)).max(1) * 2,
                    "made {} for at most {} held",
                    ring.made(),
                    model.len().max(warm)
                );
                assert_eq!(
                    std::iter::from_fn(|| ring.pop()).collect::<Vec<_>>(),
                    model.drain(..).collect::<Vec<_>>()
                );
            }
        }
        // A long churn: a step of a linear congruential generator (Knuth's MMIX constants, TAOCP vol. 2 §3.3.4)
        // picks push or pop.
        let ring = CellRing::new(1000);
        let mut model = std::collections::VecDeque::new();
        let (mut state, mut most) = (1u64, 0usize);
        for step in 0..200_000u32 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            if (state >> 33) % 5 < 3 {
                if ring.push(step).is_ok() {
                    model.push_back(step);
                } else {
                    assert_eq!(model.len(), 1000);
                }
            } else {
                assert_eq!(ring.pop(), model.pop_front());
            }
            most = most.max(model.len());
            assert_eq!(ring.len(), model.len());
        }
        assert!(ring.made() <= most * 2, "made {} for at most {most}", ring.made());
    }

    /// kiln's goal 11: a stack's cells are made as it first holds that many, and taken again after.
    #[test]
    fn g11_a_stack_makes_its_cells_as_it_first_holds_that_many() {
        let stack = CellStack::new(100);
        assert_eq!(stack.slots.made(), 0);
        for round in 0..3 {
            for value in 0..10 {
                stack.push(value).unwrap();
            }
            assert_eq!(stack.slots.made(), 10, "round {round}");
            assert_eq!(
                std::iter::from_fn(|| stack.pop()).collect::<Vec<_>>(),
                (0..10).rev().collect::<Vec<_>>()
            );
        }
    }
}
