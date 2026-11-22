//! (a) A page table in shards with private pins (PostgreSQL's buffer manager): every access to a
//! page it isn't already holding looks the page up in its shard under the shard's lock and pins
//! the frame there, then reads the frame under its shared latch. A client keeps its last page
//! pinned in a count of its own (PostgreSQL's `PrivateRefCount`), so going back to the page it is
//! on (a scan's next tuple) touches neither the table nor the shared pin count.

use super::{Core, Map, Reach, Shards, Stats, Store};
use crate::sync::atomic::{AtomicU64, Ordering};
use crate::Result;
use std::sync::Arc;

/// (a)'s pool.
pub struct Table {
    core: Core<Shards>,
}

/// A client's private pin: the page it is on and that page's frame.
#[derive(Default)]
pub struct Pinned {
    held: Option<(u64, usize)>,
}

impl Table {
    /// `frames` frames over `store`; the table's shards from the machine ([`super::shards_for_machine`]).
    pub fn new(store: Arc<Store>, frames: usize) -> Result<Self> {
        Ok(Self {
            core: Core::new(store, frames, Shards::new())?,
        })
    }

    /// Page `no`'s frame pinned, looked up under its shard's lock.
    fn pin(&self, no: u64) -> Option<usize> {
        let s = self.core.map.shard(no)?.lock();
        let i = *s.get(&no)?;
        self.core
            .frame(i)
            .ok()?
            .pins
            .fetch_add(1, Ordering::Acquire);
        Some(i)
    }

    fn unpin(&self, i: usize) {
        if let Ok(fr) = self.core.frame(i) {
            fr.pins.fetch_sub(1, Ordering::Release);
        }
    }
}

impl Reach for Table {
    type Client = Pinned;

    fn name(&self) -> &'static str {
        "table"
    }

    fn client(&self) -> Pinned {
        Pinned::default()
    }

    fn read<R>(&self, c: &mut Pinned, no: u64, mut f: impl FnMut(&[AtomicU64]) -> R) -> Result<R> {
        if let Some((p, i)) = c.held {
            // A pinned frame isn't evicted: it still holds page `p`.
            if p == no {
                if let Some(r) = self.core.shared(i, no, &mut f) {
                    return Ok(r);
                }
            }
            self.unpin(i);
            c.held = None;
        }
        if let Some(i) = self.pin(no) {
            if let Some(r) = self.core.shared(i, no, &mut f) {
                c.held = Some((no, i));
                return Ok(r);
            }
            self.unpin(i);
        }
        let (i, r) = self.core.fetch(no, |fr| {
            fr.pins.fetch_add(1, Ordering::Acquire);
            f(&fr.words)
        })?;
        c.held = Some((no, i));
        Ok(r)
    }

    fn write<R>(&self, c: &mut Pinned, no: u64, f: impl FnOnce(&[AtomicU64]) -> R) -> Result<R> {
        let at = match c.held {
            Some((p, i)) if p == no => Some(i),
            _ => self.core.map.find(no),
        };
        Ok(self.core.write(at, no, f)?.1)
    }

    fn release(&self, c: &mut Pinned) {
        if let Some((_, i)) = c.held.take() {
            self.unpin(i);
        }
    }

    fn stats(&self) -> Stats {
        self.core.stats()
    }
}
