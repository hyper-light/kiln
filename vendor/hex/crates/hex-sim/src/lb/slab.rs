//! Generational slots (kiln `crates/kiln-ci/src/lb/slab.rs` at f20858c9c): a value lives in a slot
//! until it is taken out, and the slot is used again. Each [`Handle`] carries the generation its
//! slot had when the value went in, so a handle kept past its value (a word about a box that was
//! deleted, its slot since reused) finds nothing: no map, no hash, one comparison.

use super::{NIL, ix};

/// A value's place in a [`Slab`]: its slot, and that slot's generation when the value went in. A
/// slot's generation grows by one as a value goes in and again as it comes out, so a live value's
/// is odd and a free slot's even. It wraps after 2^31 uses of one slot: a handle would have to
/// outlive that many reuses to be mistaken for a newer one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Handle {
    pub slot: u32,
    pub generation: u32,
}

impl Handle {
    /// A handle no value has (no slot is numbered NIL, and generation 0 is a free slot's).
    pub const NONE: Handle = Handle {
        slot: NIL,
        generation: 0,
    };
}

#[derive(Debug, Clone, Copy)]
struct Slot<T> {
    generation: u32,
    /// The next free slot while this one is free.
    next: u32,
    val: T,
}

/// Values in reusable slots, each reached by its [`Handle`] in O(1). Freed slots are reused last
/// freed first.
#[derive(Debug, Clone)]
pub struct Slab<T> {
    slots: Vec<Slot<T>>,
    free: u32,
    len: u32,
}

impl<T: Copy> Default for Slab<T> {
    fn default() -> Self {
        Slab::new()
    }
}

/// Whether a slot's generation is the handle's: a live value, the same one.
#[inline]
fn matches<T>(s: &Slot<T>, h: Handle) -> bool {
    #[cfg(test)]
    if super::planted(super::Mutant::NoGeneration) {
        return s.generation & 1 == 1;
    }
    s.generation == h.generation && h.generation & 1 == 1
}

impl<T: Copy> Slab<T> {
    pub fn new() -> Slab<T> {
        Slab {
            slots: Vec::new(),
            free: NIL,
            len: 0,
        }
    }

    /// Room for `n` values before the slab grows.
    pub fn with_capacity(n: usize) -> Slab<T> {
        Slab {
            slots: Vec::with_capacity(n),
            free: NIL,
            len: 0,
        }
    }

    /// How many values it holds.
    pub fn len(&self) -> usize {
        ix(self.len)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Puts `val` in a slot, the one freed last if any. `None` once 2^32 - 1 slots are in use.
    #[inline]
    pub fn insert(&mut self, val: T) -> Option<Handle> {
        let slot = self.free;
        // NIL is past the end of any slab that can exist, so "no free slot" is a miss here.
        if let Some(s) = self.slots.get_mut(ix(slot)) {
            self.free = s.next;
            s.generation = s.generation.wrapping_add(1);
            s.val = val;
            self.len = self.len.wrapping_add(1);
            return Some(Handle {
                slot,
                generation: s.generation,
            });
        }
        let slot = u32::try_from(self.slots.len()).ok().filter(|s| *s != NIL)?;
        self.slots.push(Slot {
            generation: 1,
            next: NIL,
            val,
        });
        self.len = self.len.wrapping_add(1);
        Some(Handle {
            slot,
            generation: 1,
        })
    }

    /// The value `h` is the handle of, if it is still there.
    #[inline]
    pub fn get(&self, h: Handle) -> Option<&T> {
        self.slots
            .get(ix(h.slot))
            .filter(|s| matches(s, h))
            .map(|s| &s.val)
    }

    #[inline]
    pub fn get_mut(&mut self, h: Handle) -> Option<&mut T> {
        self.slots
            .get_mut(ix(h.slot))
            .filter(|s| matches(s, h))
            .map(|s| &mut s.val)
    }

    /// Takes out the value `h` is the handle of; its slot is free for the next value.
    #[inline]
    pub fn remove(&mut self, h: Handle) -> Option<T> {
        let s = self.slots.get_mut(ix(h.slot)).filter(|s| matches(s, h))?;
        s.generation = s.generation.wrapping_add(1);
        s.next = self.free;
        self.free = h.slot;
        self.len = self.len.wrapping_sub(1);
        Some(s.val)
    }

    /// The live value in a slot, whatever its generation: for a holder that owns the slot outright
    /// and keeps only its number.
    #[inline]
    pub fn at(&self, slot: u32) -> Option<&T> {
        self.slots
            .get(ix(slot))
            .filter(|s| s.generation & 1 == 1)
            .map(|s| &s.val)
    }

    #[inline]
    pub fn at_mut(&mut self, slot: u32) -> Option<&mut T> {
        self.slots
            .get_mut(ix(slot))
            .filter(|s| s.generation & 1 == 1)
            .map(|s| &mut s.val)
    }

    /// The handle of the live value in a slot.
    #[inline]
    pub fn handle(&self, slot: u32) -> Option<Handle> {
        self.slots
            .get(ix(slot))
            .filter(|s| s.generation & 1 == 1)
            .map(|s| Handle {
                slot,
                generation: s.generation,
            })
    }

    /// Every live value with its handle, in slot order.
    pub fn iter(&self) -> impl Iterator<Item = (Handle, &T)> {
        self.slots
            .iter()
            .zip(0u32..)
            .filter(|(s, _)| s.generation & 1 == 1)
            .map(|(s, slot)| {
                (
                    Handle {
                        slot,
                        generation: s.generation,
                    },
                    &s.val,
                )
            })
    }

    /// Every live value's handle, in slot order (for a scan that changes the values as it goes).
    pub fn handles(&self) -> Vec<Handle> {
        self.iter().map(|(h, _)| h).collect()
    }
}
