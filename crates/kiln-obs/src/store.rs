//! One replica's store: symbols, the index and the head, fed write batches in the order the group
//! committed them, and asked for series and their samples (`docs/observe.md` §4, §5).
//!
//! Applying is deterministic: every replica that applies the same batches in the same order holds
//! the same series under the same ids. A batch is applied entry by entry; an entry that does not
//! decode stops the batch there, alike on every replica (the node that built it validated it, so
//! this is corruption, which the log's own checksums catch first).

use crate::batch::{BatchError, Item, Reader};
use crate::gorilla::DecodeError;
use crate::hash::Keyed;
use crate::head::{Appended, HeadSeries};
use crate::index::{Defined, Index, IndexError, Selector, SeriesInfo};
use crate::symbols::Symbols;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    #[error(transparent)]
    Batch(#[from] BatchError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error("a series' samples: {0}")]
    Samples(#[from] DecodeError),
}

/// What applying batches did, counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Runs applied.
    pub runs: u64,
    pub new_series: u64,
    pub samples: u64,
    /// Late samples kept (a correction or a backfill).
    pub late: u64,
    /// Late samples identical to one held: a replayed write, dropped.
    pub duplicates: u64,
}

impl std::ops::AddAssign for Counts {
    fn add_assign(&mut self, o: Self) {
        self.runs += o.runs;
        self.new_series += o.new_series;
        self.samples += o.samples;
        self.late += o.late;
        self.duplicates += o.duplicates;
    }
}

/// What applying a batch did, and what its sender must act on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    pub counts: Counts,
    /// Runs of series this replica does not know: the sender defines them again.
    pub unknown: Vec<u64>,
    /// Definitions refused: a fingerprint not its key's, or one naming another series.
    pub refused: Vec<u64>,
}

#[derive(Debug)]
pub struct Store {
    symbols: Symbols,
    index: Index,
    head: Vec<HeadSeries>,
    totals: Counts,
}

impl Default for Store {
    fn default() -> Self {
        Self::new(Keyed::new())
    }
}

/// A series, as a query sees it.
#[derive(Debug, Clone, Copy)]
pub struct SeriesRef<'a> {
    store: &'a Store,
    pub id: u32,
    info: SeriesInfo,
}

impl<'a> SeriesRef<'a> {
    pub fn metric(&self) -> &'a str {
        self.store.symbols.get(self.info.metric)
    }

    pub fn kind(&self) -> crate::batch::Kind {
        self.info.kind
    }

    pub fn tags(&self) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
        let s = self.store;
        let info = self.info;
        s.index.tags(&info).iter().map(move |&(k, v)| (s.symbols.get(k), s.symbols.get(v)))
    }

    /// The value of tag `key`.
    pub fn tag(&self, key: &str) -> Option<&'a str> {
        self.tags().find(|(k, _)| *k == key).map(|(_, v)| v)
    }
}

impl Store {
    pub fn new(hash: Keyed) -> Self {
        Self { symbols: Symbols::new(hash), index: Index::new(hash), head: Vec::new(), totals: Counts::default() }
    }

    /// Applies a write batch, item by item.
    pub fn apply(&mut self, batch: &[u8]) -> Result<Applied, ApplyError> {
        let mut done = Applied::default();
        let result = self.apply_items(batch, &mut done);
        self.totals += done.counts;
        result.map(|()| done)
    }

    fn apply_items(&mut self, batch: &[u8], done: &mut Applied) -> Result<(), ApplyError> {
        let mut r = Reader::new(batch)?;
        while let Some(item) = r.next_item()? {
            match item {
                Item::Def { fp, kind, key } => match self.index.define(&mut self.symbols, fp, key, kind) {
                    Ok(Defined::New(_)) => {
                        done.counts.new_series += 1;
                        self.head.push(HeadSeries::default());
                    }
                    Ok(Defined::Known(_)) => {}
                    Err(IndexError::Collision(fp) | IndexError::WrongFingerprint(fp)) => done.refused.push(fp),
                    Err(e) => return Err(e.into()),
                },
                Item::Run { fp, samples } => {
                    let Some(series) = self.index.by_fp(fp).and_then(|id| self.head.get_mut(id as usize)) else {
                        done.unknown.push(fp);
                        continue;
                    };
                    done.counts.runs += 1;
                    let mut d = samples.iter();
                    while let Some((t, v)) = d.next_sample()? {
                        match series.append(t, v)? {
                            Appended::InOrder => {}
                            Appended::Late => done.counts.late += 1,
                            Appended::Duplicate => done.counts.duplicates += 1,
                        }
                        done.counts.samples += 1;
                    }
                }
            }
        }
        Ok(())
    }

    /// The ids of the series `sel` selects, sorted.
    pub fn select(&self, sel: &Selector, out: &mut Vec<u32>) {
        self.index.select(&self.symbols, sel, out);
    }

    pub fn series(&self, id: u32) -> Option<SeriesRef<'_>> {
        self.index.info(id).map(|info| SeriesRef { store: self, id, info })
    }

    /// Appends series `id`'s samples in `[from, to]`, in time order.
    pub fn read(&self, id: u32, from: i64, to: i64, ts: &mut Vec<i64>, values: &mut Vec<f64>) -> Result<(), DecodeError> {
        match self.head.get(id as usize) {
            Some(h) => h.read(from, to, ts, values),
            None => Ok(()),
        }
    }

    /// Every metric's name.
    pub fn metric_names(&self) -> impl Iterator<Item = &str> {
        self.index.metrics().iter().map(|&m| self.symbols.get(m))
    }

    /// Every value of tag `key`.
    pub fn tag_values(&self, key: &str) -> Vec<&str> {
        self.index.tag_values(&self.symbols, key).into_iter().map(|s| self.symbols.get(s)).collect()
    }

    pub fn totals(&self) -> Counts {
        self.totals
    }

    pub fn series_count(&self) -> usize {
        self.index.len()
    }

    /// Bytes held, by part.
    pub fn heap(&self) -> Heap {
        Heap {
            symbols: self.symbols.heap_bytes(),
            index: self.index.heap_bytes(),
            samples: self.head.iter().map(HeadSeries::heap_bytes).sum::<usize>() + self.head.capacity() * std::mem::size_of::<HeadSeries>(),
        }
    }

    pub fn kind_conflicts(&self) -> u64 {
        self.index.kind_conflicts
    }
}

/// What a store holds in memory, by part.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Heap {
    pub symbols: usize,
    pub index: usize,
    pub samples: usize,
}

impl Heap {
    pub fn total(&self) -> usize {
        self.symbols + self.index + self.samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::{Builder, Kind, SeriesKey};

    #[test]
    fn written_samples_are_selected_and_read_back() {
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let a = SeriesKey::new("system.cpu.user", [("host", "a"), ("env", "prod")], Kind::Gauge).unwrap();
        let b_key = SeriesKey::new("system.cpu.user", [("host", "b"), ("env", "prod")], Kind::Gauge).unwrap();
        let m = SeriesKey::new("system.mem.used", [("host", "a")], Kind::Gauge).unwrap();
        let mut b = Builder::new();
        for k in [&a, &b_key, &m] {
            b.define(k);
        }
        b.run(a.fp, (0..10).map(|i| (i * 1000, i as f64)));
        b.run(b_key.fp, (0..10).map(|i| (i * 1000, 100.0 + i as f64)));
        b.run(m.fp, [(0, 1e9)]);
        let applied = s.apply(&b.finish()).unwrap();
        assert_eq!(applied.counts, Counts { runs: 3, new_series: 3, samples: 21, late: 0, duplicates: 0 });
        let mut ids = Vec::new();
        s.select(&Selector::metric("system.cpu.user").tag("env", "prod"), &mut ids);
        assert_eq!(ids.len(), 2);
        let b_id = ids.iter().copied().find(|&id| s.series(id).and_then(|r| r.tag("host")) == Some("b")).unwrap();
        let (mut ts, mut vs) = (Vec::new(), Vec::new());
        s.read(b_id, 2000, 4000, &mut ts, &mut vs).unwrap();
        assert_eq!((ts.clone(), vs.clone()), (vec![2000, 3000, 4000], vec![102.0, 103.0, 104.0]));
        // A later batch names the series by fingerprint alone; an earlier sample is a late write.
        let mut b = Builder::new();
        b.run(b_key.fp, [(10_000, 110.0), (500, 0.5)]);
        let applied = s.apply(&b.finish()).unwrap();
        assert_eq!((applied.counts.new_series, applied.counts.late), (0, 1));
        ts.clear();
        vs.clear();
        s.read(b_id, 0, 1000, &mut ts, &mut vs).unwrap();
        assert_eq!(ts, vec![0, 500, 1000]);
        assert_eq!(s.metric_names().collect::<Vec<_>>(), vec!["system.cpu.user", "system.mem.used"]);
        let mut hosts = s.tag_values("host");
        hosts.sort_unstable();
        assert_eq!(hosts, vec!["a", "b"]);
    }

    #[test]
    fn a_run_of_an_unknown_series_is_reported_and_a_bad_definition_refused() {
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let a = SeriesKey::new("m", [("host", "a")], Kind::Gauge).unwrap();
        let mut b = Builder::new();
        b.run(a.fp, [(1, 1.0)]);
        let applied = s.apply(&b.finish()).unwrap();
        assert_eq!(applied.unknown, vec![a.fp]);
        assert_eq!(s.series_count(), 0);
        // Defined again, as the sender does on that answer: the run lands.
        b.define(&a);
        b.run(a.fp, [(1, 1.0)]);
        let applied = s.apply(&b.finish()).unwrap();
        assert!(applied.unknown.is_empty());
        assert_eq!(applied.counts.samples, 1);
        // A definition whose fingerprint is not its key's is refused.
        let mut forged = SeriesKey::new("m", [("host", "evil")], Kind::Gauge).unwrap();
        forged.fp = a.fp;
        b.define(&forged);
        let applied = s.apply(&b.finish()).unwrap();
        assert_eq!(applied.refused, vec![a.fp]);
        assert_eq!(s.series_count(), 1);
    }

    #[test]
    fn replicas_that_apply_the_same_batches_hold_the_same_state() {
        let mut batches = Vec::new();
        let mut rng = crate::testkit::Rng::new(77);
        let mut b = Builder::new();
        for round in 0..20 {
            for _ in 0..30 {
                let host = format!("h{}", rng.below(50));
                let metric = format!("m{}", rng.below(5));
                let k = SeriesKey::new(&metric, [("host", host.as_str())], Kind::Gauge).unwrap();
                b.define(&k);
                b.run(k.fp, (0..5).map(|i| (round * 100 + i * 10 + rng.below(3) as i64, rng.f64())));
            }
            batches.push(b.finish());
        }
        // Different hash keys: different table layouts, the same ids and contents.
        let mut a = Store::new(Keyed::with_keys(1, 2));
        let mut b = Store::new(Keyed::with_keys(9, 9));
        for batch in &batches {
            a.apply(batch).unwrap();
            b.apply(batch).unwrap();
        }
        assert_eq!(a.series_count(), b.series_count());
        for id in 0..a.series_count() as u32 {
            let (ra, rb) = (a.series(id).unwrap(), b.series(id).unwrap());
            assert_eq!(ra.metric(), rb.metric());
            assert_eq!(ra.tags().collect::<Vec<_>>(), rb.tags().collect::<Vec<_>>());
            let (mut ta, mut va, mut tb, mut vb) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            a.read(id, i64::MIN, i64::MAX, &mut ta, &mut va).unwrap();
            b.read(id, i64::MIN, i64::MAX, &mut tb, &mut vb).unwrap();
            assert_eq!(ta, tb);
            assert_eq!(va.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), vb.iter().map(|v| v.to_bits()).collect::<Vec<_>>());
        }
    }
}
