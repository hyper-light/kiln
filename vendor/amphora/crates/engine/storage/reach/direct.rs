//! (c) A direct array (vmcache's translation without the virtual memory): one slot per page of
//! the file, sized from the file, holding the page's frame; a read loads the slot and reads the
//! frame optimistically, as (b) does. No table, no lock and no per-client state on a hit; the
//! array costs four bytes a page of the file whether the page is cached or not.

use super::{Core, Map, Reach, Stats, Store};
use crate::error::LimboError;
use crate::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use crate::Result;
use std::sync::Arc;

/// A slot with no frame.
const EMPTY: u32 = u32::MAX;

/// Page `n`'s frame at slot `n - 1`.
pub(crate) struct Slots {
    slots: Box<[AtomicU32]>,
}

impl Slots {
    fn slot(&self, no: u64) -> Option<&AtomicU32> {
        self.slots.get(usize::try_from(no.checked_sub(1)?).ok()?)
    }
}

impl Map for Slots {
    fn find(&self, no: u64) -> Option<usize> {
        let s = self.slot(no)?.load(Ordering::Acquire);
        (s != EMPTY).then(|| usize::try_from(s).ok()).flatten()
    }

    fn insert(&self, no: u64, frame: usize) {
        if let (Some(s), Ok(f)) = (self.slot(no), u32::try_from(frame)) {
            s.store(f, Ordering::Release);
        }
    }

    fn take(&self, no: u64, _frame: &super::Frame) -> bool {
        if let Some(s) = self.slot(no) {
            s.store(EMPTY, Ordering::Release);
        }
        true
    }
}

/// (c)'s pool.
pub struct Direct {
    core: Core<Slots>,
}

impl Direct {
    /// `frames` frames over `store`, and a slot for each of its pages. A pool of more frames than
    /// a slot can name is refused.
    pub fn new(store: Arc<Store>, frames: usize) -> Result<Self> {
        if u32::try_from(frames).ok().is_none_or(|f| f == EMPTY) {
            return Err(LimboError::InvalidArgument(format!(
                "{frames} frames are more than a direct slot names"
            )));
        }
        let pages = usize::try_from(store.pages()).map_err(|_| {
            LimboError::InvalidArgument("the file's pages don't fit an array here".into())
        })?;
        let mut v = Vec::new();
        v.try_reserve_exact(pages)
            .map_err(|_| LimboError::OutOfMemory)?;
        v.extend((0..pages).map(|_| AtomicU32::new(EMPTY)));
        Ok(Self {
            core: Core::new(
                store,
                frames,
                Slots {
                    slots: v.into_boxed_slice(),
                },
            )?,
        })
    }
}

impl Reach for Direct {
    type Client = ();

    fn name(&self) -> &'static str {
        "direct"
    }

    fn client(&self) {}

    fn read<R>(&self, _c: &mut (), no: u64, mut f: impl FnMut(&[AtomicU64]) -> R) -> Result<R> {
        if let Some(i) = self.core.map.find(no) {
            if let Some(r) = self.core.read_at(i, no, &mut f) {
                return Ok(r);
            }
        }
        Ok(self.core.fetch(no, |fr| f(&fr.words))?.1)
    }

    fn write<R>(&self, _c: &mut (), no: u64, f: impl FnOnce(&[AtomicU64]) -> R) -> Result<R> {
        Ok(self.core.write(self.core.map.find(no), no, f)?.1)
    }

    fn release(&self, _c: &mut ()) {}

    fn stats(&self) -> Stats {
        self.core.stats()
    }
}
