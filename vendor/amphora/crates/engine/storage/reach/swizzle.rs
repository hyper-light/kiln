//! (b) References by frame index, read optimistically (LeanStore, Umbra): a client keeps, for each
//! page it reached, the frame it found the page in (standing in for a parent page's swizzled
//! pointer), and reads that frame without a lock or a pin: the frame's version before and after,
//! the read thrown away and done again under the latch if a writer was there. A reference whose
//! frame now holds another page (it was evicted) goes cold and is found again through the table,
//! as LeanStore finds an unswizzled child.

use super::{Core, Map, Reach, Shards, Stats, Store};
use crate::sync::atomic::AtomicU64;
use crate::Result;
use std::sync::Arc;

/// (b)'s pool.
pub struct Swizzle {
    core: Core<Shards>,
}

/// A client's references: for page `n`, its frame plus one (0: cold), grown as pages are reached.
#[derive(Default)]
pub struct Swips {
    swips: Vec<u32>,
}

impl Swips {
    fn get(&self, no: u64) -> Option<usize> {
        let s = *self.swips.get(usize::try_from(no).ok()?)?;
        usize::try_from(s.checked_sub(1)?).ok()
    }

    fn set(&mut self, no: u64, frame: Option<usize>) {
        let Ok(n) = usize::try_from(no) else { return };
        let s = frame
            .and_then(|f| u32::try_from(f).ok())
            .and_then(|f| f.checked_add(1))
            .unwrap_or(0);
        if n >= self.swips.len() {
            if s == 0 {
                return;
            }
            self.swips.resize(n.saturating_add(1), 0);
        }
        if let Some(slot) = self.swips.get_mut(n) {
            *slot = s;
        }
    }
}

impl Swizzle {
    /// `frames` frames over `store`; the cold table's shards from the machine.
    pub fn new(store: Arc<Store>, frames: usize) -> Result<Self> {
        Ok(Self {
            core: Core::new(store, frames, Shards::new())?,
        })
    }
}

impl Reach for Swizzle {
    type Client = Swips;

    fn name(&self) -> &'static str {
        "swizzle"
    }

    fn client(&self) -> Swips {
        // A reference for every page of the file, at most: the client's array grows to the
        // highest page it reaches.
        Swips::default()
    }

    fn read<R>(&self, c: &mut Swips, no: u64, mut f: impl FnMut(&[AtomicU64]) -> R) -> Result<R> {
        if let Some(i) = c.get(no) {
            if let Some(r) = self.core.read_at(i, no, &mut f) {
                return Ok(r);
            }
            c.set(no, None);
        }
        if let Some(i) = self.core.map.find(no) {
            if let Some(r) = self.core.read_at(i, no, &mut f) {
                c.set(no, Some(i));
                return Ok(r);
            }
        }
        let (i, r) = self.core.fetch(no, |fr| f(&fr.words))?;
        c.set(no, Some(i));
        Ok(r)
    }

    fn write<R>(&self, c: &mut Swips, no: u64, f: impl FnOnce(&[AtomicU64]) -> R) -> Result<R> {
        let at = c.get(no).or_else(|| self.core.map.find(no));
        let (i, r) = self.core.write(at, no, f)?;
        c.set(no, Some(i));
        Ok(r)
    }

    fn release(&self, _c: &mut Swips) {}

    fn stats(&self) -> Stats {
        self.core.stats()
    }
}
