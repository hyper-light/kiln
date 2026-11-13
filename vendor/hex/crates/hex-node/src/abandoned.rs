//! Each box's abandoned programs (decision 3's disconnect rule: exec'd programs the node ended because their
//! caller's connection closed), counted into the box's record, which inspect reports. One copy for every OS's
//! node (native plan step 8). A count a record write can't take now is kept, reported all the same, and
//! written with the next, never dropped (CLAUDE.md §3). Each box's count has a lock of its own, held across the
//! write that takes it, so one box's durable write holds up no other box's inspect or create (goal 5 plan
//! step 2). An entry goes with its box's delete.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use hex_api::{BoxId, Refusal};

use crate::store::Store;

/// Each box's count not yet in its record, each behind a lock of its own.
type Pending = BTreeMap<BoxId, Arc<Mutex<u64>>>;

/// The counts not yet in their boxes' records, by box.
#[derive(Default)]
pub struct Abandoned {
    pending: Mutex<Pending>,
}

fn poisoned() -> Refusal {
    Refusal::Broken("the node's abandoned-program counts' lock was poisoned".to_owned())
}

impl Abandoned {
    fn pending(&self) -> Result<MutexGuard<'_, Pending>, Refusal> {
        self.pending.lock().map_err(|_| poisoned())
    }

    /// Counts one more in box `id`, and writes every count it holds for it to the box's record in `store`.
    /// A box with no readable record (deleted since, or a run's removed once its program ended, decision 7) has
    /// none to take it, and nothing is kept for it.
    pub fn count(&self, store: &Store, id: &BoxId) -> Result<(), Refusal> {
        let entry = Arc::clone(self.pending()?.entry(id.clone()).or_default());
        let mut count = entry.lock().map_err(|_| poisoned())?;
        // Saturating: 2^64 programs is past any box's life, so the cap is never met.
        *count = count.saturating_add(1);
        let add = *count;
        let recorded = store.update(id, |r| {
            r.abandoned = r.abandoned.saturating_add(add);
            true
        })?;
        *count = 0;
        if !recorded {
            // Its box is gone, so its entry goes too: under the count's own lock, which a count of a box made since
            // under its id waits for, so no count of that box is lost with it.
            let mut pending = self.pending()?;
            if pending
                .get(id)
                .is_some_and(|held| Arc::ptr_eq(held, &entry))
            {
                pending.remove(id);
            }
        }
        Ok(())
    }

    /// Box `id`'s count not yet in its record; waits only for a write of its own count.
    pub fn unrecorded(&self, id: &BoxId) -> Result<u64, Refusal> {
        let Some(count) = self.pending()?.get(id).map(Arc::clone) else {
            return Ok(0);
        };
        let count = count.lock().map_err(|_| poisoned())?;
        Ok(*count)
    }

    /// Box `id` was deleted: nothing more to count for it.
    pub fn forget(&self, id: &BoxId) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use hex_api::{Bounds, BoxSpec, ClassSpec};
    use hex_core::record::{BoxRecord, Phase};

    use super::*;

    /// CLAUDE.md §2 (runtime goal 7): a program abandoned in a box that is gone by the time its count comes (a run's
    /// box, removed once its program ended, or a box deleted since) has no record to take the count. Do: count one in
    /// a box with no record, and one in a box with a record. Expect: nothing kept for the first, which held an entry
    /// for good before; the second's count in its record.
    #[test]
    fn a_count_no_record_takes_keeps_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("disk"), &dir.path().join("run")).unwrap();
        let (gone, kept) = (BoxId::new("gone").unwrap(), BoxId::new("kept").unwrap());
        store
            .put(&BoxRecord::new(
                BoxSpec {
                    route: None,
                    allocation: None,
                    devices: Vec::new(),
                    id: kept.clone(),
                    bounds: Bounds::default(),
                    class: ClassSpec::Process,
                    owner: None,
                    band: 0,
                },
                Phase::Ready,
                Vec::new(),
            ))
            .unwrap();
        let abandoned = Abandoned::default();
        abandoned.count(&store, &gone).unwrap();
        abandoned.count(&store, &kept).unwrap();
        assert!(
            !abandoned.pending.lock().unwrap().contains_key(&gone),
            "a count no record took is kept"
        );
        assert_eq!(abandoned.unrecorded(&gone).unwrap(), 0);
        let Some(Ok(record)) = store.get(&kept).unwrap() else {
            panic!("the kept box's record is gone");
        };
        assert_eq!(record.abandoned, 1);
    }
}
