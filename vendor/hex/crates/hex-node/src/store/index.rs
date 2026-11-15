//! The store's records in memory (the review's per-window reads, step 2; ROOT, 2026-11-14): every box record and node VM
//! row its directory holds, read once as the store opens and changed by the store alone, in the same turn as each write
//! of its file. The store is its directory's one writer ([`super::lock`]), so this is the directory's state, not a cache
//! of it: nothing else writes a record, and a file changed behind the store's back is out of its contract, seen by the
//! next store's open. Every read of a record is this index's: a window's read of every record is one pointer
//! ([`Snapshot`]), a box's or a row's one keyed lookup.
//!
//! Its bound is the records' own: at most [`TURNS`] box records (a new box's first write refused past it, typed) and
//! [`TURNS`] node VM rows, each within the record bound ([`hex_core::record::MAX_RECORD_BYTES`]) and held as decoded, so
//! its memory is about the records' size on disk (a few KB each).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use hex_api::{BoxId, NodeRun, Refusal};
use hex_core::record::{BoxRecord, NodeVmRecord};

use super::TURNS;

/// A box's record as its store holds it: the record, or why its file can't be read.
pub type Entry = Result<BoxRecord, String>;

/// Every box record a store holds at one moment, by box id. Shared: a reader holds pointers, never copies, and a write
/// made while a reader holds one replaces the map of pointers, never a record.
pub type Snapshot = Arc<BTreeMap<Arc<BoxId>, Arc<Entry>>>;

/// The records, under one lock held for the in-memory change alone, never across a file's write.
#[derive(Debug, Default)]
pub(super) struct Index(Mutex<Kept>);

#[derive(Debug, Default)]
struct Kept {
    boxes: Snapshot,
    /// New boxes whose first write is under way, each holding a place under the box bound until its record is in
    /// `boxes` or its write has failed.
    making: usize,
    rows: BTreeMap<NodeRun, NodeVmRecord>,
}

/// A box's place under the box bound for its first write, given back as it drops unless its record took it.
pub(super) struct Place<'a> {
    index: &'a Index,
    held: bool,
}

impl Drop for Place<'_> {
    fn drop(&mut self) {
        if self.held {
            // Nothing panics holding the lock (CLAUDE.md §1), so a poisoned one still holds whole counts.
            let mut kept = self.index.0.lock().unwrap_or_else(PoisonError::into_inner);
            kept.making = kept.making.saturating_sub(1);
        }
    }
}

fn poisoned() -> Refusal {
    Refusal::Broken("the store's records in memory are poisoned".to_owned())
}

impl Index {
    fn kept(&self) -> Result<MutexGuard<'_, Kept>, Refusal> {
        self.0.lock().map_err(|_| poisoned())
    }

    /// Takes `boxes` and `rows`, the open's scan of the directory, as the records it holds.
    pub(super) fn load(
        &self,
        boxes: BTreeMap<BoxId, Entry>,
        rows: Vec<NodeVmRecord>,
    ) -> Result<(), Refusal> {
        let mut held = BTreeMap::new();
        for (id, entry) in boxes {
            held.insert(Arc::new(id), Arc::new(entry));
        }
        let mut by_run = BTreeMap::new();
        for row in rows {
            by_run.insert(row.run, row);
        }
        let mut kept = self.kept()?;
        kept.boxes = Arc::new(held);
        kept.rows = by_run;
        Ok(())
    }

    /// Every box record held now, shared.
    pub(super) fn snapshot(&self) -> Result<Snapshot, Refusal> {
        Ok(Arc::clone(&self.kept()?.boxes))
    }

    /// Box `id`'s record, where one is held.
    pub(super) fn entry(&self, id: &BoxId) -> Result<Option<Arc<Entry>>, Refusal> {
        Ok(self.kept()?.boxes.get(id).cloned())
    }

    /// Box `id`'s place for a write of its record: none needed where its record is held; a new box takes one under the
    /// box bound, refused past it, so boxes made at once never pass it together (the review's B-14).
    pub(super) fn admit(&self, id: &BoxId) -> Result<Place<'_>, Refusal> {
        let mut kept = self.kept()?;
        if kept.boxes.contains_key(id) {
            return Ok(Place {
                index: self,
                held: false,
            });
        }
        let taken = kept
            .boxes
            .len()
            .checked_add(kept.making)
            .ok_or(Refusal::Full(TURNS))?;
        if taken >= TURNS {
            return Err(Refusal::Full(TURNS));
        }
        kept.making = kept.making.checked_add(1).ok_or(Refusal::Full(TURNS))?;
        Ok(Place {
            index: self,
            held: true,
        })
    }

    /// Holds `entry` as box `id`'s record, its file now holding it, and gives `place` back in the same change.
    pub(super) fn publish(
        &self,
        id: &BoxId,
        entry: Entry,
        place: &mut Place<'_>,
    ) -> Result<(), Refusal> {
        let key = Arc::new(id.try_clone()?);
        let mut kept = self.kept()?;
        Arc::make_mut(&mut kept.boxes).insert(key, Arc::new(entry));
        if place.held {
            kept.making = kept.making.saturating_sub(1);
            place.held = false;
        }
        Ok(())
    }

    /// Holds `entry` as box `id`'s record, or none where `entry` is none: what its files hold after a change cut short.
    pub(super) fn reset(&self, id: &BoxId, entry: Option<Entry>) -> Result<(), Refusal> {
        match entry {
            Some(entry) => self.publish(
                id,
                entry,
                &mut Place {
                    index: self,
                    held: false,
                },
            ),
            None => self.forget(id),
        }
    }

    /// Lets box `id`'s record go, its files removed.
    pub(super) fn forget(&self, id: &BoxId) -> Result<(), Refusal> {
        let mut kept = self.kept()?;
        if kept.boxes.contains_key(id) {
            Arc::make_mut(&mut kept.boxes).remove(id);
        }
        Ok(())
    }

    /// Node VM `run`'s row, where one is held.
    pub(super) fn row(&self, run: &NodeRun) -> Result<Option<NodeVmRecord>, Refusal> {
        Ok(self.kept()?.rows.get(run).cloned())
    }

    /// Every node VM row held, in their runs' order.
    pub(super) fn rows(&self) -> Result<Vec<NodeVmRecord>, Refusal> {
        let kept = self.kept()?;
        let mut rows = Vec::new();
        rows.try_reserve_exact(kept.rows.len())
            .map_err(|_| Refusal::Exhausted {
                what: "node VM record storage",
                bound: kept.rows.len(),
            })?;
        rows.extend(kept.rows.values().cloned());
        Ok(rows)
    }

    /// How many node VM rows are held.
    pub(super) fn row_count(&self) -> Result<usize, Refusal> {
        Ok(self.kept()?.rows.len())
    }

    /// Holds `row` as its run's, its file now holding it.
    pub(super) fn set_row(&self, row: NodeVmRecord) -> Result<(), Refusal> {
        self.kept()?.rows.insert(row.run, row);
        Ok(())
    }

    /// Lets node VM `run`'s row go, its file removed.
    pub(super) fn drop_row(&self, run: &NodeRun) -> Result<(), Refusal> {
        self.kept()?.rows.remove(run);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The review's B-14 (batch 1114d's reads2; AC-T7.4.1): a new box takes its place under the box bound as its first
    /// write starts, counted with every box being made, so boxes made at once never pass the bound together. Do take a
    /// place for every box but one being made; hold one of them as written; take places for two more new boxes, and one
    /// for the box held; give one back and take the refused one again. Expect the first of the two given the last place
    /// and the second refused Full at the bound, nothing counted for it; the held box needing no place; and the refused
    /// box given the place given back.
    #[test]
    fn reads2_b14_boxes_made_at_once_never_pass_the_box_bound_together() {
        let index = Index::default();
        let id = |n: usize| BoxId::new(&format!("b{n}")).unwrap();
        let mut places: Vec<Place<'_>> = (0..TURNS - 1)
            .map(|n| index.admit(&id(n)).unwrap())
            .collect();
        index
            .publish(&id(0), Err("its file".to_owned()), &mut places[0])
            .unwrap();
        let last = index.admit(&id(TURNS)).unwrap();
        assert_eq!(
            index.admit(&id(TURNS + 1)).err(),
            Some(Refusal::Full(TURNS))
        );
        assert!(
            !index.admit(&id(0)).unwrap().held,
            "a held box took a place"
        );
        drop(last);
        assert!(index.admit(&id(TURNS + 1)).unwrap().held);
    }
}
