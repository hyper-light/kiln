//! The head: each series' recent samples in memory, Gorilla-encoded as they arrive
//! (`docs/observe.md` §4.3, §4.5).
//!
//! Samples newer than a series' last go into its open chunk. A late sample (at or before the last)
//! is one of three things:
//! - a duplicate of one held (same time, same value bits): a retried or replayed write, dropped, so
//!   writes are idempotent;
//! - a correction (same time, another value) or a backfill (a time not held): kept as the later
//!   write, in the series' late buffer, sorted; a full buffer becomes a sorted chunk of its own;
//! - nothing is refused. Reads merge the late chunks over the in-order ones, the later write
//!   winning at a time; and once a series' late samples reach its in-order ones, the series is
//!   rewritten in order, so the merge stays cheap and the rewrites cost O(1) a sample amortised
//!   (each rewrite at least doubles what is in order, the dynamic-array argument).

use crate::gorilla::{Chunk, ChunkRef, DecodeError, Encoder};

/// Samples a chunk holds before it is closed: the shortest length within 2% of the compression
/// plateau on recorded data (`docs/observe.md` §12.2).
pub const CHUNK_SAMPLES: u32 = 240;

/// Late samples a series buffers before they become a chunk: Prometheus's default out-of-order
/// chunk capacity, `DefaultOutOfOrderCapMax` (32, tsdb's `head.go`).
pub const LATE_BUFFER: usize = 32;

/// A series' head.
#[derive(Debug, Default)]
pub struct HeadSeries {
    open: Encoder,
    closed: Vec<Chunk>,
    late: Option<Box<Late>>,
}

/// A series' late writes, oldest first.
#[derive(Debug, Default)]
struct Late {
    chunks: Vec<Chunk>,
    /// Sorted by time; at a time, the latest write.
    buffer: Vec<(i64, f64)>,
    samples: u64,
}

/// What appending a sample did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appended {
    InOrder,
    /// Late, kept as the later write.
    Late,
    /// Late and identical to a sample held: dropped.
    Duplicate,
}

impl HeadSeries {
    fn last(&self) -> Option<i64> {
        self.open.span().map(|(_, l)| l).or_else(|| self.closed.last().map(|c| c.last))
    }

    #[inline]
    pub fn append(&mut self, ts: i64, value: f64) -> Result<Appended, DecodeError> {
        if self.last().is_some_and(|last| ts <= last) {
            return self.append_late(ts, value);
        }
        if self.open.count() >= CHUNK_SAMPLES {
            let full = std::mem::replace(&mut self.open, Encoder::with_capacity(chunk_capacity()));
            self.closed.push(full.finish());
        }
        // After the last: the push cannot be refused.
        let _ = self.open.push(ts, value);
        Ok(Appended::InOrder)
    }

    fn append_late(&mut self, ts: i64, value: f64) -> Result<Appended, DecodeError> {
        if self.holds(ts, value)? {
            return Ok(Appended::Duplicate);
        }
        let in_order = self.in_order_samples();
        let late = self.late.get_or_insert_with(Box::default);
        match late.buffer.binary_search_by_key(&ts, |s| s.0) {
            Ok(i) => {
                if let Some(s) = late.buffer.get_mut(i) {
                    s.1 = value;
                }
            }
            Err(i) => {
                late.buffer.insert(i, (ts, value));
                late.samples += 1;
            }
        }
        if late.buffer.len() >= LATE_BUFFER {
            let mut e = Encoder::with_capacity(LATE_BUFFER * 2);
            for &(t, v) in &late.buffer {
                // Sorted and distinct: the push cannot be refused.
                let _ = e.push(t, v);
            }
            late.chunks.push(e.finish());
            late.buffer.clear();
        }
        if late.samples >= in_order {
            self.rewrite()?;
        }
        Ok(Appended::Late)
    }

    /// Whether the newest write at `ts` is exactly `value`.
    fn holds(&self, ts: i64, value: f64) -> Result<bool, DecodeError> {
        let want = value.to_bits();
        if let Some(late) = &self.late {
            if let Ok(i) = late.buffer.binary_search_by_key(&ts, |s| s.0) {
                return Ok(late.buffer.get(i).is_some_and(|s| s.1.to_bits() == want));
            }
            for c in late.chunks.iter().rev() {
                if (c.first..=c.last).contains(&ts)
                    && let Some(v) = find(c.as_ref(), ts)?
                {
                    return Ok(v.to_bits() == want);
                }
            }
        }
        if let Some((f, l)) = self.open.span()
            && (f..=l).contains(&ts)
        {
            return Ok(find(self.open.view(), ts)?.is_some_and(|v| v.to_bits() == want));
        }
        let i = self.closed.partition_point(|c| c.last < ts);
        match self.closed.get(i) {
            Some(c) if c.first <= ts => Ok(find(c.as_ref(), ts)?.is_some_and(|v| v.to_bits() == want)),
            _ => Ok(false),
        }
    }

    fn in_order_samples(&self) -> u64 {
        self.closed.iter().map(|c| u64::from(c.count)).sum::<u64>() + u64::from(self.open.count())
    }

    /// Rewrites the series in order, its late writes merged in.
    fn rewrite(&mut self) -> Result<(), DecodeError> {
        let (mut ts, mut vs) = (Vec::new(), Vec::new());
        self.read(i64::MIN, i64::MAX, &mut ts, &mut vs)?;
        self.closed.clear();
        self.open = Encoder::with_capacity(chunk_capacity());
        self.late = None;
        for (t, v) in ts.into_iter().zip(vs) {
            if self.open.count() >= CHUNK_SAMPLES {
                let full = std::mem::replace(&mut self.open, Encoder::with_capacity(chunk_capacity()));
                self.closed.push(full.finish());
            }
            let _ = self.open.push(t, v);
        }
        Ok(())
    }

    /// Appends the samples in `[from, to]`, in time order, to `ts` and `values`; at a time, the
    /// latest write.
    pub fn read(&self, from: i64, to: i64, ts: &mut Vec<i64>, values: &mut Vec<f64>) -> Result<(), DecodeError> {
        let start = ts.len();
        let first = self.closed.partition_point(|c| c.last < from);
        for c in self.closed.iter().skip(first) {
            if c.first > to {
                break;
            }
            c.as_ref().decode_range_into((c.first, c.last), from, to, ts, values)?;
        }
        if let Some((f, l)) = self.open.span()
            && l >= from
            && f <= to
        {
            self.open.view().decode_range_into((f, l), from, to, ts, values)?;
        }
        if let Some(late) = &self.late {
            merge_late(late, start, from, to, ts, values)?;
        }
        Ok(())
    }

    /// The first and last times held.
    pub fn span(&self) -> Option<(i64, i64)> {
        let mut first: Option<i64> = self.closed.first().map(|c| c.first).or_else(|| self.open.span().map(|s| s.0));
        let mut last: Option<i64> = self.open.span().map(|s| s.1).or_else(|| self.closed.last().map(|c| c.last));
        if let Some(late) = &self.late {
            for (f, l) in late.chunks.iter().map(|c| (c.first, c.last)).chain(late.buffer.first().zip(late.buffer.last()).map(|(a, b)| (a.0, b.0))) {
                first = Some(first.map_or(f, |x| x.min(f)));
                last = Some(last.map_or(l, |x| x.max(l)));
            }
        }
        first.zip(last)
    }

    /// Takes the series' samples as chunks in time order that do not overlap (late writes merged
    /// in first), leaving the series empty: what a cut writes to a block.
    pub fn drain_chunks(&mut self) -> Result<Vec<Chunk>, DecodeError> {
        if self.late.is_some() {
            self.rewrite()?;
        }
        let mut chunks = std::mem::take(&mut self.closed);
        let open = std::mem::take(&mut self.open);
        if open.count() > 0 {
            chunks.push(open.finish());
        }
        Ok(chunks)
    }

    /// Puts back what [`HeadSeries::drain_chunks`] took (a cut that failed to write).
    pub fn restore(&mut self, chunks: Vec<Chunk>) {
        self.closed = chunks;
        self.open = Encoder::new();
    }

    /// Samples held (late ones that correct a held time counted once each).
    pub fn samples(&self) -> u64 {
        self.in_order_samples() + self.late.as_ref().map_or(0, |l| l.samples)
    }

    pub fn chunks(&self) -> usize {
        self.closed.len() + usize::from(self.open.count() > 0) + self.late.as_ref().map_or(0, |l| l.chunks.len())
    }

    /// Bytes held: chunks as allocated, the late writes, and the series' own struct.
    pub fn heap_bytes(&self) -> usize {
        let chunks = |v: &Vec<Chunk>| v.iter().map(|c| c.bytes.capacity()).sum::<usize>() + v.capacity() * std::mem::size_of::<Chunk>();
        chunks(&self.closed)
            + self.open.capacity()
            + self.late.as_ref().map_or(0, |l| std::mem::size_of::<Late>() + chunks(&l.chunks) + l.buffer.capacity() * 16)
    }
}

/// The open chunk's first allocation: the bytes a full chunk takes at the corpus's median rate
/// (1.167 bytes a sample, §12.1), so most chunks never reallocate.
fn chunk_capacity() -> usize {
    (CHUNK_SAMPLES as usize * 1167).div_ceil(1000)
}

/// The value at `ts` in a chunk.
fn find(c: ChunkRef<'_>, ts: i64) -> Result<Option<f64>, DecodeError> {
    let mut d = c.iter();
    while let Some((t, v)) = d.next_sample()? {
        if t == ts {
            return Ok(Some(v));
        }
        if t > ts {
            break;
        }
    }
    Ok(None)
}

/// Merges a series' late writes in `[from, to]` into the in-order run `ts[start..]`: at a time the
/// latest write wins (the buffer, then the late chunks newest first, then the run).
fn merge_late(late: &Late, start: usize, from: i64, to: i64, ts: &mut Vec<i64>, values: &mut Vec<f64>) -> Result<(), DecodeError> {
    // (time, rank, value): rank 0 the run, 1.. the chunks oldest first, the buffer last.
    let mut all: Vec<(i64, u32, f64)> = Vec::new();
    for (&t, &v) in ts.get(start..).unwrap_or(&[]).iter().zip(values.get(start..).unwrap_or(&[])) {
        all.push((t, 0, v));
    }
    let mut rank = 1u32;
    for c in &late.chunks {
        if c.last >= from && c.first <= to {
            let mut d = c.as_ref().iter();
            while let Some((t, v)) = d.next_sample()? {
                if (from..=to).contains(&t) {
                    all.push((t, rank, v));
                }
            }
        }
        rank += 1;
    }
    for &(t, v) in &late.buffer {
        if (from..=to).contains(&t) {
            all.push((t, rank, v));
        }
    }
    all.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    ts.truncate(start);
    values.truncate(start);
    let mut i = 0;
    while let Some(&(t, _, _)) = all.get(i) {
        let mut j = i;
        while all.get(j + 1).is_some_and(|n| n.0 == t) {
            j += 1;
        }
        if let Some(&(_, _, v)) = all.get(j) {
            ts.push(t);
            values.push(v);
        }
        i = j + 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Rng;

    #[test]
    fn a_series_reads_back_what_was_written_across_chunks_and_ranges() {
        let mut s = HeadSeries::default();
        for i in 0..1000i64 {
            assert_eq!(s.append(i * 10, i as f64).unwrap(), Appended::InOrder);
        }
        assert_eq!(s.chunks(), 1000usize.div_ceil(CHUNK_SAMPLES as usize));
        let (mut ts, mut vs) = (Vec::new(), Vec::new());
        s.read(i64::MIN, i64::MAX, &mut ts, &mut vs).unwrap();
        assert_eq!(ts.len(), 1000);
        ts.clear();
        vs.clear();
        s.read(2395, 2405, &mut ts, &mut vs).unwrap();
        assert_eq!((ts, vs), (vec![2400], vec![240.0]));
    }

    #[test]
    fn a_replayed_write_is_a_duplicate_and_a_correction_wins() {
        let mut s = HeadSeries::default();
        for t in [10, 20, 40, 50] {
            s.append(t, t as f64).unwrap();
        }
        // Replayed: dropped.
        for t in [10, 20, 40, 50] {
            assert_eq!(s.append(t, t as f64).unwrap(), Appended::Duplicate);
        }
        assert_eq!(s.append(30, 30.0).unwrap(), Appended::Late);
        assert_eq!(s.append(30, 30.0).unwrap(), Appended::Duplicate);
        assert_eq!(s.append(20, 99.0).unwrap(), Appended::Late);
        let (mut ts, mut vs) = (Vec::new(), Vec::new());
        s.read(0, 100, &mut ts, &mut vs).unwrap();
        assert_eq!(ts, vec![10, 20, 30, 40, 50]);
        assert_eq!(vs, vec![10.0, 99.0, 30.0, 40.0, 50.0]);
    }

    #[test]
    fn a_whole_series_written_twice_stays_one_and_a_large_backfill_is_rewritten_in_order() {
        let mut s = HeadSeries::default();
        for i in 0..2000i64 {
            s.append(i * 10_000, i as f64).unwrap();
        }
        for i in 0..2000i64 {
            assert_eq!(s.append(i * 10_000, i as f64).unwrap(), Appended::Duplicate);
        }
        assert!(s.late.is_none());
        // A backfill between every sample: as many late as in order, so the series is rewritten.
        for i in 0..2000i64 {
            s.append(i * 10_000 + 5_000, -1.0).unwrap();
        }
        assert!(s.late.as_ref().is_none_or(|l| l.samples < s.in_order_samples()));
        let (mut ts, mut vs) = (Vec::new(), Vec::new());
        s.read(i64::MIN, i64::MAX, &mut ts, &mut vs).unwrap();
        assert_eq!(ts.len(), 4000);
        assert!(ts.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn reads_agree_with_a_plain_model_under_random_writes() {
        let mut rng = Rng::new(17);
        for _ in 0..300 {
            let mut s = HeadSeries::default();
            let mut model = std::collections::BTreeMap::new();
            let mut t = 0i64;
            for _ in 0..rng.below(1500) {
                if rng.below(4) == 0 && t > 0 {
                    let late = rng.below(t as u64 + 1) as i64;
                    let v = if rng.below(2) == 0 { model.get(&late).copied().unwrap_or(0.5) } else { rng.f64() };
                    s.append(late, v).unwrap();
                    model.insert(late, v);
                } else {
                    t += 1 + rng.below(50) as i64;
                    let v = rng.f64();
                    s.append(t, v).unwrap();
                    model.insert(t, v);
                }
            }
            let from = rng.below(t.max(1) as u64) as i64;
            let to = from + rng.below(30_000) as i64;
            let (mut ts, mut vs) = (Vec::new(), Vec::new());
            s.read(from, to, &mut ts, &mut vs).unwrap();
            let want: Vec<(i64, u64)> = model.range(from..=to).map(|(&k, &v)| (k, v.to_bits())).collect();
            let got: Vec<(i64, u64)> = ts.into_iter().zip(vs).map(|(t, v)| (t, v.to_bits())).collect();
            assert_eq!(got, want);
        }
    }
}
