//! Rollups (`docs/observe.md` §4.7): a series' samples aggregated a window of time at a time, kept
//! in its blocks beside its chunks, so a query whose buckets are whole windows reads a window's
//! aggregates instead of its samples.
//!
//! Replicas cut and merge their blocks at their own times, so which windows a replica holds rolled
//! up is its own; its answers are not. A query whose buckets are whole windows of a tier defines a
//! bucket window by window: each window's samples folded in time order ([`Agg`]), then the
//! windows' folds in time order. A window is read from its rollup or from its samples alike
//! (`query::eval`), and so floating-point addition, which does not associate, gives every replica
//! the same answer to the bit.
//!
//! ```text
//! rollups := tiers u8 | per tier, finest first: byte length varint | tier
//! tier    := width ms varint | windows varint | windows' byte length varint
//!            | per window: its index (the first's zigzag, a later one's gap − 1) varint
//!                          | its samples varint (0: one is not finite, and its samples are read)
//!            | per column of its kind: byte length varint | the counted windows' values, XOR'd
//! ```
//!
//! The lengths let a reader find a tier and its columns without reading what it skips.
//!
//! A column is Gorilla's XOR of each window's value with the one before (`crate::gorilla`): a
//! series that holds its level writes a bit a window.

use crate::batch::Kind;
use crate::bits::{BitReader, BitWriter, Bits, Truncated};
use crate::gorilla::{read_xor, write_xor};
use crate::varint;

/// The tiers' window widths, finest first: five minutes and an hour, Thanos's downsampling
/// resolutions (Mimir's and Cortex's too). Every step of five minutes or more on the query grid's
/// ladder (`query::eval::STEPS`) is a multiple of five minutes, every one of an hour or more a
/// multiple of an hour, and a grid begins at a multiple of its step: a long range's buckets are
/// whole windows, as a calendar's days are in every timezone in use (their offsets are whole
/// quarter hours).
pub const TIERS: [i64; 2] = [300_000, 3_600_000];

/// A tier is kept for a series when it takes at most `1 / KEEP_RATIO` of the bytes of the samples
/// it stands for: a query it serves reads that much less (`docs/observe.md` §12.16 measures the
/// choice).
pub const KEEP_RATIO: usize = 4;

/// A column of a window's aggregates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col {
    Sum,
    Min,
    Max,
    Last,
    First,
    /// A counter's increases after the window's first sample.
    Inc,
}

/// The columns a kind's rollups read: a gauge's or a count's sum, least, most and last; a
/// counter's first, last and increases (every rollup of a counter is of its increases).
pub fn columns(kind: Kind) -> &'static [Col] {
    match kind {
        Kind::Counter => &[Col::First, Col::Last, Col::Inc],
        Kind::Gauge | Kind::Count => &[Col::Sum, Col::Min, Col::Max, Col::Last],
    }
}

/// `a` and then `b`, NaN as nothing: how a window's values, and a bucket's windows, are summed.
#[inline]
pub fn join(a: f64, b: f64) -> f64 {
    if b.is_nan() {
        a
    } else if a.is_nan() {
        b
    } else {
        a + b
    }
}

/// A counter's increase from `p` to `v`: a fall is a reset, and the increase the new value
/// (Prometheus's counter semantics).
#[inline]
pub fn increase(p: f64, v: f64) -> f64 {
    if v >= p { v - p } else { v }
}

/// A window's aggregates of its samples, every one finite, in time order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Agg {
    pub n: u32,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
    pub first: f64,
    pub last: f64,
    /// The increases from each sample to the next, joined; NaN with one sample.
    pub inc: f64,
}

impl Agg {
    #[inline]
    pub fn of(v: f64) -> Self {
        Self { n: 1, sum: v, min: v, max: v, first: v, last: v, inc: f64::NAN }
    }

    #[inline]
    pub fn push(&mut self, v: f64) {
        self.n += 1;
        self.sum = join(self.sum, v);
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        self.inc = join(self.inc, increase(self.last, v));
        self.last = v;
    }

    fn get(&self, c: Col) -> f64 {
        match c {
            Col::Sum => self.sum,
            Col::Min => self.min,
            Col::Max => self.max,
            Col::Last => self.last,
            Col::First => self.first,
            Col::Inc => self.inc,
        }
    }
}

/// Each window a series has samples in, `(index, aggregates)`; `None` for one with a sample that is
/// not finite. Its index: the window's start over its width.
fn windows(width: i64, ts: &[i64], vs: &[f64], out: &mut Vec<(i64, Option<Agg>)>) {
    out.clear();
    let mut cur: Option<(i64, Option<Agg>)> = None;
    for (&t, &v) in ts.iter().zip(vs) {
        let k = t.div_euclid(width);
        match &mut cur {
            Some((ck, agg)) if *ck == k => {
                if let Some(a) = agg {
                    if v.is_finite() {
                        a.push(v);
                    } else {
                        *agg = None;
                    }
                }
            }
            _ => {
                if let Some(w) = cur.take() {
                    out.push(w);
                }
                cur = Some((k, v.is_finite().then(|| Agg::of(v))));
            }
        }
    }
    if let Some(w) = cur {
        out.push(w);
    }
}

/// Room [`build`] keeps from series to series.
#[derive(Debug, Default)]
pub struct Builder {
    windows: Vec<(i64, Option<Agg>)>,
    tier: Vec<u8>,
    col: BitWriter,
}

/// `kind`'s rollups of samples `ts` and `vs` (in time order, each time once) into `out`, cleared:
/// each tier of [`TIERS`] that takes at most `raw_bytes / KEEP_RATIO` (the bytes the samples take),
/// nothing when none does.
pub fn build(kind: Kind, ts: &[i64], vs: &[f64], raw_bytes: usize, b: &mut Builder, out: &mut Vec<u8>) {
    out.clear();
    out.push(0);
    let mut kept = 0u8;
    for &width in &TIERS {
        windows(width, ts, vs, &mut b.windows);
        encode_tier(kind, width, &b.windows, &mut b.col, &mut b.tier);
        if b.windows.is_empty() || b.tier.len().saturating_mul(KEEP_RATIO) > raw_bytes {
            continue;
        }
        out.extend_from_slice(&b.tier);
        kept += 1;
    }
    if kept == 0 {
        out.clear();
    } else if let Some(n) = out.first_mut() {
        *n = kept;
    }
}

/// Each tier's windows and bytes for these samples, kept or not: what [`build`] weighs.
pub fn sizes(kind: Kind, ts: &[i64], vs: &[f64], b: &mut Builder) -> [(usize, usize); TIERS.len()] {
    let mut out = [(0, 0); TIERS.len()];
    for (o, &width) in out.iter_mut().zip(&TIERS) {
        windows(width, ts, vs, &mut b.windows);
        encode_tier(kind, width, &b.windows, &mut b.col, &mut b.tier);
        *o = (b.windows.len(), b.tier.len());
    }
    out
}

/// A tier, its byte length before it.
fn encode_tier(kind: Kind, width: i64, windows: &[(i64, Option<Agg>)], col: &mut BitWriter, out: &mut Vec<u8>) {
    out.clear();
    let mut body = std::mem::take(out);
    let mut list = Vec::with_capacity(windows.len() * 2);
    let mut prev = None;
    for &(k, a) in windows {
        match prev {
            None => varint::put(&mut list, varint::zig(k)),
            Some(p) => varint::put(&mut list, (k.wrapping_sub(p) as u64).wrapping_sub(1)),
        }
        prev = Some(k);
        varint::put(&mut list, a.map_or(0, |a| u64::from(a.n)));
    }
    varint::put(&mut body, width as u64);
    varint::put(&mut body, windows.len() as u64);
    varint::put(&mut body, list.len() as u64);
    body.extend_from_slice(&list);
    let out_body = &mut body;
    for &c in columns(kind) {
        col.clear();
        let (mut last, mut window) = (None, None);
        for a in windows.iter().filter_map(|w| w.1) {
            let bits = a.get(c).to_bits();
            match last {
                None => col.write(bits, 64),
                Some(p) => write_xor(col, p, bits, &mut window),
            }
            last = Some(bits);
        }
        varint::put(out_body, col.byte_len() as u64);
        col.write_to(out_body);
    }
    varint::put(out, body.len() as u64);
    out.extend_from_slice(&body);
}

/// Rollups that do not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a series' rollups are damaged")]
pub struct Damaged;

impl From<Truncated> for Damaged {
    fn from(_: Truncated) -> Self {
        Self
    }
}

/// A series' tier of `width`, from its rollups as a block holds them; none when it has none.
pub fn tier(rollups: &[u8], width: i64, kind: Kind) -> Result<Option<Tier<'_>>, Damaged> {
    let mut at = rollups;
    let Some((&tiers, rest)) = at.split_first() else { return Ok(None) };
    at = rest;
    for _ in 0..tiers {
        let len = varint::get(&mut at).and_then(|n| usize::try_from(n).ok()).ok_or(Damaged)?;
        let (mut body, rest) = at.split_at_checked(len).ok_or(Damaged)?;
        at = rest;
        let w = varint::get(&mut body).and_then(|w| i64::try_from(w).ok()).ok_or(Damaged)?;
        if w == width {
            return Tier::parse(w, body, kind).map(Some);
        }
    }
    Ok(None)
}

/// Whether `rollups` are what samples `ts` and `vs` make of every window of every tier they hold,
/// to the bit (which tiers they hold is the writer's to choose).
pub fn check(kind: Kind, ts: &[i64], vs: &[f64], rollups: &[u8]) -> Result<(), Damaged> {
    let mut at = rollups;
    let Some((&tiers, rest)) = at.split_first() else { return Ok(()) };
    at = rest;
    let mut want = Vec::new();
    let mut got = Rolled::default();
    let mut seen: Vec<i64> = Vec::new();
    for _ in 0..tiers {
        let len = varint::get(&mut at).and_then(|n| usize::try_from(n).ok()).ok_or(Damaged)?;
        let (mut body, rest) = at.split_at_checked(len).ok_or(Damaged)?;
        at = rest;
        let width = varint::get(&mut body).and_then(|w| i64::try_from(w).ok()).ok_or(Damaged)?;
        if seen.contains(&width) {
            return Err(Damaged);
        }
        seen.push(width);
        let tier = Tier::parse(width, body, kind)?;
        windows(width, ts, vs, &mut want);
        got.clear();
        tier.read(i64::MIN, i64::MAX, columns(kind), &mut got)?;
        if got.len() != want.len() {
            return Err(Damaged);
        }
        for (i, &(k, agg)) in want.iter().enumerate() {
            let same = got.start.get(i) == Some(&k.saturating_mul(width))
                && match agg {
                    None => got.n.get(i) == Some(&0),
                    Some(a) => got.agg(i).is_some_and(|g| g.n == a.n && columns(kind).iter().all(|&c| g.get(c).to_bits() == a.get(c).to_bits())),
                };
            if !same {
                return Err(Damaged);
            }
        }
    }
    if at.is_empty() { Ok(()) } else { Err(Damaged) }
}

/// One tier of a series' rollups, parsed as far as where its parts lie.
#[derive(Debug, Clone, Copy)]
pub struct Tier<'a> {
    pub width: i64,
    windows: usize,
    /// Each window's index and count, as varints.
    list: &'a [u8],
    cols: [(Col, &'a [u8]); 4],
    ncols: usize,
}

impl<'a> Tier<'a> {
    /// A tier's body after its width.
    fn parse(width: i64, mut at: &'a [u8], kind: Kind) -> Result<Self, Damaged> {
        if width <= 0 {
            return Err(Damaged);
        }
        let mut get = || varint::get(&mut at).and_then(|n| usize::try_from(n).ok()).ok_or(Damaged);
        let windows = get()?;
        let list_len = get()?;
        // Every window takes a byte of its index and one of its count at the least.
        if windows > list_len / 2 {
            return Err(Damaged);
        }
        let (list, mut rest) = at.split_at_checked(list_len).ok_or(Damaged)?;
        let mut cols = [(Col::Sum, &[][..]); 4];
        let kinds = columns(kind);
        for (slot, &c) in cols.iter_mut().zip(kinds) {
            let len = varint::get(&mut rest).and_then(|n| usize::try_from(n).ok()).ok_or(Damaged)?;
            let (bytes, after) = rest.split_at_checked(len).ok_or(Damaged)?;
            rest = after;
            *slot = (c, bytes);
        }
        Ok(Self { width, windows, list, cols, ncols: kinds.len() })
    }

    /// Appends to `out` each window that lies in `[from, to]` (ms) in part or whole: its start, its
    /// samples and, of a counted one, the values of `want` (NaN for one not counted); a column not
    /// wanted is neither read nor filled.
    pub fn read(&self, from: i64, to: i64, want: &[Col], out: &mut Rolled) -> Result<(), Damaged> {
        let mut list = self.list;
        let (lo, hi) = (from.div_euclid(self.width), to.div_euclid(self.width));
        let mut readers: [Option<(BitReader<'a>, u64, (u32, u32), bool)>; 4] = [None, None, None, None];
        for (r, &(c, bytes)) in readers.iter_mut().zip(self.cols.iter().take(self.ncols)) {
            if want.contains(&c) {
                *r = Some((BitReader::new(Bits::bytes(bytes)), 0, (0, 0), false));
            }
        }
        let mut k = 0i64;
        for i in 0..self.windows {
            let v = varint::get(&mut list).ok_or(Damaged)?;
            k = if i == 0 { varint::zag(v) } else { k.wrapping_add(v as i64).wrapping_add(1) };
            let n = u32::try_from(varint::get(&mut list).ok_or(Damaged)?).map_err(|_| Damaged)?;
            let inside = k >= lo && k <= hi;
            if inside {
                out.start.push(k.saturating_mul(self.width));
                out.n.push(n);
            }
            for (slot, &(c, _)) in readers.iter_mut().zip(self.cols.iter().take(self.ncols)) {
                let Some((r, prev, window, started)) = slot else { continue };
                let value = if n > 0 {
                    let bits = if *started { read_xor(r, *prev, window)? } else { r.read(64)? };
                    *started = true;
                    *prev = bits;
                    f64::from_bits(bits)
                } else {
                    f64::NAN
                };
                if inside {
                    out.col_mut(c).push(value);
                }
            }
            if k > hi {
                break;
            }
        }
        Ok(())
    }
}

/// Windows read from rollups, a column each: a column a reader did not want is empty (its values
/// NaN to [`Rolled::agg`]).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Rolled {
    pub start: Vec<i64>,
    pub n: Vec<u32>,
    pub sum: Vec<f64>,
    pub min: Vec<f64>,
    pub max: Vec<f64>,
    pub last: Vec<f64>,
    pub first: Vec<f64>,
    pub inc: Vec<f64>,
}

impl Rolled {
    /// Room for `n` windows (a range over a tier's width, and one) in the columns `cols`: a reader's
    /// columns grown once.
    pub fn with_capacity(n: usize, cols: &[Col]) -> Self {
        let mut r = Self { start: Vec::with_capacity(n), n: Vec::with_capacity(n), ..Self::default() };
        for &c in cols {
            r.col_mut(c).reserve(n);
        }
        r
    }

    pub fn clear(&mut self) {
        self.truncate(0);
    }

    pub fn len(&self) -> usize {
        self.start.len()
    }

    pub fn is_empty(&self) -> bool {
        self.start.is_empty()
    }

    pub fn truncate(&mut self, len: usize) {
        self.start.truncate(len);
        self.n.truncate(len);
        for c in [Col::Sum, Col::Min, Col::Max, Col::Last, Col::First, Col::Inc] {
            self.col_mut(c).truncate(len);
        }
    }

    pub fn col(&self, c: Col) -> &[f64] {
        match c {
            Col::Sum => &self.sum,
            Col::Min => &self.min,
            Col::Max => &self.max,
            Col::Last => &self.last,
            Col::First => &self.first,
            Col::Inc => &self.inc,
        }
    }

    fn col_mut(&mut self, c: Col) -> &mut Vec<f64> {
        match c {
            Col::Sum => &mut self.sum,
            Col::Min => &mut self.min,
            Col::Max => &mut self.max,
            Col::Last => &mut self.last,
            Col::First => &mut self.first,
            Col::Inc => &mut self.inc,
        }
    }

    /// Window `i`'s aggregates (of a counted window).
    pub fn agg(&self, i: usize) -> Option<Agg> {
        let n = *self.n.get(i)?;
        let get = |c: Col| self.col(c).get(i).copied().unwrap_or(f64::NAN);
        (n > 0).then(|| Agg { n, sum: get(Col::Sum), min: get(Col::Min), max: get(Col::Max), first: get(Col::First), last: get(Col::Last), inc: get(Col::Inc) })
    }

    /// Keeps window `from` as window `to` (`to <= from`): how a reader drops the windows it does
    /// not take, in place.
    pub fn keep(&mut self, from: usize, to: usize) {
        fn mv<T: Copy>(v: &mut [T], from: usize, to: usize) {
            if let Some(&x) = v.get(from)
                && let Some(slot) = v.get_mut(to)
            {
                *slot = x;
            }
        }
        mv(&mut self.start, from, to);
        mv(&mut self.n, from, to);
        for c in [Col::Sum, Col::Min, Col::Max, Col::Last, Col::First, Col::Inc] {
            mv(self.col_mut(c), from, to);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::testkit::Rng;

    /// A day of a series every `every` ms from `start`, jittered and with gaps, `kind`'s values.
    fn series(rng: &mut Rng, kind: Kind, start: i64, every: i64, n: usize) -> (Vec<i64>, Vec<f64>) {
        let (mut ts, mut vs) = (Vec::new(), Vec::new());
        let mut t = start;
        let mut v = 100.0;
        for _ in 0..n {
            t += every + (rng.below(5) as i64) - 2;
            if rng.below(50) == 0 {
                t += every * (1 + rng.below(40) as i64);
            }
            v = match kind {
                Kind::Counter if rng.below(500) == 0 => rng.f64() * 3.0,
                Kind::Counter => v + rng.f64() * 10.0,
                _ => v + rng.f64() - 0.5,
            };
            ts.push(t);
            vs.push(v);
        }
        (ts, vs)
    }

    #[test]
    fn every_tier_reads_back_each_window_s_aggregates_exactly() {
        let mut rng = Rng::new(7);
        let mut b = Builder::default();
        let mut out = Vec::new();
        for kind in [Kind::Gauge, Kind::Count, Kind::Counter] {
            for start in [-86_400_000i64, 0, 1_700_000_123_456] {
                let (ts, mut vs) = series(&mut rng, kind, start, 10_000, 5_000);
                vs[17] = f64::NAN;
                vs[2_000] = f64::INFINITY;
                // Room enough to keep every tier: what is read back is what is checked.
                build(kind, &ts, &vs, usize::MAX / KEEP_RATIO, &mut b, &mut out);
                for &width in &TIERS {
                    let tier = tier(&out, width, kind).unwrap().unwrap();
                    let mut want = Vec::new();
                    windows(width, &ts, &vs, &mut want);
                    let mut got = Rolled::default();
                    tier.read(i64::MIN / 2, i64::MAX / 2, columns(kind), &mut got).unwrap();
                    assert_eq!(got.len(), want.len());
                    for (i, (k, agg)) in want.iter().enumerate() {
                        assert_eq!(got.start[i], k * width);
                        match agg {
                            None => assert_eq!(got.n[i], 0, "a window with a sample not finite is not counted"),
                            Some(a) => {
                                let g = got.agg(i).unwrap();
                                assert_eq!(g.n, a.n);
                                for &c in columns(kind) {
                                    assert_eq!(g.get(c).to_bits(), a.get(c).to_bits(), "{kind:?} {width} window {k} {c:?}");
                                }
                            }
                        }
                    }
                    // A range in the middle: the windows that touch it, no more.
                    let (from, to) = (ts[1_000], ts[3_000]);
                    let mut part = Rolled::default();
                    tier.read(from, to, &[Col::Last], &mut part).unwrap();
                    let inside: Vec<i64> = want.iter().map(|w| w.0 * width).filter(|&s| s + width > from && s <= to).collect();
                    assert_eq!(part.start, inside);
                    assert!(part.sum.iter().all(|v| v.is_nan()), "a column not asked for is not read");
                }
            }
        }
    }

    #[test]
    fn a_tier_is_kept_only_when_it_is_a_quarter_of_its_samples_or_less() {
        let mut rng = Rng::new(9);
        let mut b = Builder::default();
        let mut out = Vec::new();
        // A level held: every window's column a bit; both tiers kept.
        let ts: Vec<i64> = (0..8_640).map(|i| i * 10_000).collect();
        let vs = vec![3.0; ts.len()];
        build(Kind::Gauge, &ts, &vs, ts.len() * 2, &mut b, &mut out);
        assert_eq!(out.first(), Some(&2));
        // Noise sampled once a minute: five samples a five-minute window take less than the
        // window's aggregates would; only the hour's is kept.
        let ts: Vec<i64> = (0..1_440).map(|i| i * 60_000).collect();
        let vs: Vec<f64> = (0..ts.len()).map(|_| rng.f64()).collect();
        build(Kind::Gauge, &ts, &vs, ts.len() * 8, &mut b, &mut out);
        assert_eq!(out.first(), Some(&1));
        assert!(tier(&out, 300_000, Kind::Gauge).unwrap().is_none());
        assert!(tier(&out, 3_600_000, Kind::Gauge).unwrap().is_some());
        // A sample a day: no tier pays.
        build(Kind::Gauge, &[0, 86_400_000], &[1.0, 2.0], 20, &mut b, &mut out);
        assert!(out.is_empty());
        assert!(tier(&out, 300_000, Kind::Gauge).unwrap().is_none());
    }

    #[test]
    fn damage_is_an_error_never_a_panic() {
        let mut rng = Rng::new(11);
        let mut b = Builder::default();
        let mut out = Vec::new();
        let (ts, vs) = series(&mut rng, Kind::Counter, 0, 10_000, 3_000);
        build(Kind::Counter, &ts, &vs, usize::MAX / KEEP_RATIO, &mut b, &mut out);
        for cut in 0..out.len() {
            for &w in &TIERS {
                if let Ok(Some(t)) = tier(&out[..cut], w, Kind::Counter) {
                    let _ = t.read(i64::MIN / 2, i64::MAX / 2, columns(Kind::Counter), &mut Rolled::default());
                }
            }
        }
        for _ in 0..2_000 {
            let mut bad = out.clone();
            let i = rng.below(bad.len() as u64) as usize;
            bad[i] ^= 1 << rng.below(8);
            for &w in &TIERS {
                if let Ok(Some(t)) = tier(&bad, w, Kind::Counter) {
                    let _ = t.read(i64::MIN / 2, i64::MAX / 2, columns(Kind::Counter), &mut Rolled::default());
                }
            }
        }
    }
}
