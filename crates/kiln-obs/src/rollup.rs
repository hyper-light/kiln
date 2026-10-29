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

/// A tier's windows as a series' samples come, one at a time (time order): each window it has
/// samples in, `(index, aggregates)`, `None` for one with a sample that is not finite. Its index:
/// the window's start over its width.
#[derive(Debug)]
struct Windows {
    width: i64,
    cur: Option<(i64, Option<Agg>)>,
    done: Vec<(i64, Option<Agg>)>,
}

impl Windows {
    fn new(width: i64) -> Self {
        Self { width, cur: None, done: Vec::new() }
    }

    fn clear(&mut self) {
        self.cur = None;
        self.done.clear();
    }

    #[inline]
    fn push(&mut self, t: i64, v: f64) {
        let k = t.div_euclid(self.width);
        match &mut self.cur {
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
                if let Some(w) = self.cur.take() {
                    self.done.push(w);
                }
                self.cur = Some((k, v.is_finite().then(|| Agg::of(v))));
            }
        }
    }

    /// Every window, the last one's samples taken.
    fn finish(&mut self) -> &[(i64, Option<Agg>)] {
        if let Some(w) = self.cur.take() {
            self.done.push(w);
        }
        &self.done
    }
}

/// Each window a series has samples in (`Windows`), of a tier of `width`, at once.
fn windows(width: i64, ts: &[i64], vs: &[f64], out: &mut Vec<(i64, Option<Agg>)>) {
    let mut w = Windows { width, cur: None, done: std::mem::take(out) };
    w.clear();
    for (&t, &v) in ts.iter().zip(vs) {
        w.push(t, v);
    }
    w.finish();
    *out = w.done;
}

/// A series' rollups as its samples come ([`Builder::push`], then [`Builder::finish`]): every
/// tier's windows in one pass, a window's aggregates kept and not its samples, so a merge builds a
/// series' rollups as it reads its chunks, holding its windows (a week's: 2,016 and 168), not its
/// samples. Its room is kept from series to series.
#[derive(Debug)]
pub struct Builder {
    tiers: [Windows; TIERS.len()],
    tier: Vec<u8>,
    col: BitWriter,
    list: Vec<u8>,
    body: Vec<u8>,
}

impl Default for Builder {
    fn default() -> Self {
        Self { tiers: TIERS.map(Windows::new), tier: Vec::new(), col: BitWriter::default(), list: Vec::new(), body: Vec::new() }
    }
}

impl Builder {
    /// A series begun: what the last one left, forgotten.
    pub fn begin(&mut self) {
        for t in &mut self.tiers {
            t.clear();
        }
    }

    /// The series' next sample, in time order and each time once.
    #[inline]
    pub fn push(&mut self, t: i64, v: f64) {
        for w in &mut self.tiers {
            w.push(t, v);
        }
    }

    /// The series' rollups into `out`, cleared: each tier of [`TIERS`] that takes at most
    /// `raw_bytes / KEEP_RATIO` (the bytes its samples take), nothing when none does.
    pub fn finish(&mut self, kind: Kind, raw_bytes: usize, out: &mut Vec<u8>) {
        out.clear();
        out.push(0);
        let mut kept = 0u8;
        let Self { tiers, tier, col, list, body } = self;
        for w in tiers.iter_mut() {
            let width = w.width;
            let done = w.finish();
            encode_tier(kind, width, done, col, list, body, tier);
            if done.is_empty() || tier.len().saturating_mul(KEEP_RATIO) > raw_bytes {
                continue;
            }
            out.extend_from_slice(tier);
            kept += 1;
        }
        if kept == 0 {
            out.clear();
        } else if let Some(n) = out.first_mut() {
            *n = kept;
        }
    }
}

/// A series' rollups joined at a merge from its inputs' (`docs/observe.md` §4.7), each input's span
/// after the one before, rather than made anew from every sample: each input's tiers' windows read
/// and appended; a window a cut fell inside (the last input's last and this one's first the same
/// window) carried on with this input's samples in it, its fold continued in time order, so every
/// window is what its samples make of it, to the bit, as [`Builder`] makes it. Joined only where
/// every input keeps the same tiers ([`Joiner::add`] says when not: the merge builds them anew).
/// Its room is kept from series to series.
#[derive(Debug, Default)]
pub struct Joiner {
    widths: Vec<i64>,
    tiers: [Vec<(i64, Option<Agg>)>; TIERS.len()],
    inputs: usize,
    rolled: Rolled,
    tier: Vec<u8>,
    col: BitWriter,
    list: Vec<u8>,
    body: Vec<u8>,
}

impl Joiner {
    /// A series begun: what the last one left, forgotten.
    pub fn begin(&mut self) {
        self.widths.clear();
        for t in &mut self.tiers {
            t.clear();
        }
        self.inputs = 0;
    }

    /// The series' next input's rollups (`rollups`, of `kind`, as its block keeps them), and its
    /// samples in time order (`next`), read only as far as a window a cut fell inside needs: false
    /// where the input keeps none, or other tiers than the inputs before it.
    pub fn add(&mut self, kind: Kind, rollups: &[u8], next: &mut dyn FnMut() -> Option<(i64, f64)>) -> Result<bool, Damaged> {
        let Some((&count, mut at)) = rollups.split_first() else { return Ok(false) };
        let n = usize::from(count);
        if n == 0 {
            return Ok(false);
        }
        if n > TIERS.len() {
            return Err(Damaged);
        }
        let mut found: [(i64, &[u8]); TIERS.len()] = [(0, &[]); TIERS.len()];
        for slot in found.iter_mut().take(n) {
            let len = varint::get(&mut at).and_then(|n| usize::try_from(n).ok()).ok_or(Damaged)?;
            let (mut body, rest) = at.split_at_checked(len).ok_or(Damaged)?;
            at = rest;
            let width = varint::get(&mut body).and_then(|w| i64::try_from(w).ok()).ok_or(Damaged)?;
            *slot = (width, body);
        }
        if !at.is_empty() {
            return Err(Damaged);
        }
        let found = found.get(..n).unwrap_or_default();
        if self.inputs == 0 {
            self.widths.extend(found.iter().map(|&(w, _)| w));
        } else if !self.widths.iter().copied().eq(found.iter().map(|&(w, _)| w)) {
            return Ok(false);
        }
        // Each tier's windows appended; where this input's first is the window the last ended
        // inside, that window's place, to carry on with this input's samples in it.
        let mut carried: [Option<usize>; TIERS.len()] = [None; TIERS.len()];
        let mut until = i64::MIN;
        for ((&(width, body), out), carry) in found.iter().zip(self.tiers.iter_mut()).zip(carried.iter_mut()) {
            let tier = Tier::parse(width, body, kind)?;
            self.rolled.clear();
            tier.read(i64::MIN, i64::MAX, columns(kind), &mut self.rolled)?;
            for i in 0..self.rolled.len() {
                let k = self.rolled.start.get(i).copied().ok_or(Damaged)?.div_euclid(width);
                if i == 0 && out.last().is_some_and(|l| l.0 == k) {
                    *carry = Some(out.len() - 1);
                    until = until.max(k.saturating_add(1).saturating_mul(width));
                    continue;
                }
                out.push((k, self.rolled.agg(i)));
            }
        }
        // The carried windows' folds continued with this input's samples in them.
        if until > i64::MIN {
            while let Some((t, v)) = next() {
                if t >= until {
                    break;
                }
                for ((&(width, _), out), carry) in found.iter().zip(self.tiers.iter_mut()).zip(&carried) {
                    let Some((k, agg)) = carry.and_then(|c| out.get_mut(c)) else { continue };
                    if t.div_euclid(width) != *k {
                        continue;
                    }
                    if let Some(a) = agg {
                        if v.is_finite() {
                            a.push(v);
                        } else {
                            *agg = None;
                        }
                    }
                }
            }
        }
        self.inputs += 1;
        Ok(true)
    }

    /// The joined rollups into `out`, cleared: each tier the inputs keep, where it takes at most
    /// `raw_bytes / KEEP_RATIO` (the bytes its samples take); nothing when none does.
    pub fn finish(&mut self, kind: Kind, raw_bytes: usize, out: &mut Vec<u8>) {
        out.clear();
        out.push(0);
        let mut kept = 0u8;
        let Self { widths, tiers, tier, col, list, body, .. } = self;
        for (&width, done) in widths.iter().zip(tiers.iter()) {
            encode_tier(kind, width, done, col, list, body, tier);
            if done.is_empty() || tier.len().saturating_mul(KEEP_RATIO) > raw_bytes {
                continue;
            }
            out.extend_from_slice(tier);
            kept += 1;
        }
        if kept == 0 {
            out.clear();
        } else if let Some(n) = out.first_mut() {
            *n = kept;
        }
    }
}

/// `kind`'s rollups of samples `ts` and `vs` (in time order, each time once) into `out`, cleared
/// ([`Builder::finish`]).
pub fn build(kind: Kind, ts: &[i64], vs: &[f64], raw_bytes: usize, b: &mut Builder, out: &mut Vec<u8>) {
    b.begin();
    for (&t, &v) in ts.iter().zip(vs) {
        b.push(t, v);
    }
    b.finish(kind, raw_bytes, out);
}

/// Each tier's windows and bytes for these samples, kept or not: what [`build`] weighs.
pub fn sizes(kind: Kind, ts: &[i64], vs: &[f64], b: &mut Builder) -> [(usize, usize); TIERS.len()] {
    let mut out = [(0, 0); TIERS.len()];
    let mut done = Vec::new();
    for (o, &width) in out.iter_mut().zip(&TIERS) {
        windows(width, ts, vs, &mut done);
        let Builder { tier, col, list, body, .. } = b;
        encode_tier(kind, width, &done, col, list, body, tier);
        *o = (done.len(), b.tier.len());
    }
    out
}

/// `windows` as a tier of `width` into `tier`, its byte length before it.
fn encode_tier(kind: Kind, width: i64, windows: &[(i64, Option<Agg>)], col: &mut BitWriter, list: &mut Vec<u8>, body: &mut Vec<u8>, tier: &mut Vec<u8>) {
    list.clear();
    let mut prev = None;
    for &(k, a) in windows.iter() {
        match prev {
            None => varint::put(list, varint::zig(k)),
            Some(p) => varint::put(list, (k.wrapping_sub(p) as u64).wrapping_sub(1)),
        }
        prev = Some(k);
        varint::put(list, a.map_or(0, |a| u64::from(a.n)));
    }
    body.clear();
    varint::put(body, width as u64);
    varint::put(body, windows.len() as u64);
    varint::put(body, list.len() as u64);
    body.extend_from_slice(list);
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
        varint::put(body, col.byte_len() as u64);
        col.write_to(body);
    }
    tier.clear();
    varint::put(tier, body.len() as u64);
    tier.extend_from_slice(body);
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

    /// Rollups joined from a series' inputs are, byte for byte, what its samples make of them
    /// whole: each kind, cut into parts at random times (inside windows, on their edges, parts
    /// lying inside one window), samples that are not finite among them, counters that reset; each
    /// part's samples read only as far as a window a cut fell inside. Inputs of other tiers, or
    /// none, are not joined.
    #[test]
    fn rollups_joined_from_a_series_parts_are_its_samples_rollups() {
        let mut rng = Rng::new(17);
        let (mut b, mut j) = (Builder::default(), Joiner::default());
        let (mut whole, mut joined, mut part) = (Vec::new(), Vec::new(), Vec::new());
        for round in 0..200 {
            let kind = [Kind::Gauge, Kind::Count, Kind::Counter][round % 3];
            let every = [1_000, 10_000, 30_000, 299_999][rng.below(4) as usize];
            let (start, n) = (1_700_006_400_000 + rng.below(3_600_000) as i64, 50 + rng.below(3_000) as usize);
            let (ts, mut vs) = series(&mut rng, kind, start, every, n);
            for v in &mut vs {
                if rng.below(400) == 0 {
                    *v = if rng.below(2) == 0 { f64::NAN } else { f64::INFINITY };
                }
            }
            // Cuts: random places, some a part of one sample or of a window's few.
            let mut cuts: Vec<usize> = (0..1 + rng.below(5)).map(|_| 1 + rng.below(ts.len() as u64 - 1) as usize).collect();
            if rng.below(3) == 0 {
                let c = 1 + rng.below(ts.len() as u64 - 2) as usize;
                cuts.extend([c, c + 1]);
            }
            cuts.sort_unstable();
            cuts.dedup();
            build(kind, &ts, &vs, usize::MAX, &mut b, &mut whole);
            j.begin();
            let mut from = 0;
            for &to in cuts.iter().chain([&ts.len()]) {
                build(kind, &ts[from..to], &vs[from..to], usize::MAX, &mut b, &mut part);
                let mut i = from;
                let mut next = || {
                    let s = (i < to).then(|| (ts[i], vs[i]));
                    i += 1;
                    s
                };
                assert!(j.add(kind, &part, &mut next).unwrap(), "round {round}");
                // Read only through the windows a cut fell inside (the last one's end), and the
                // sample that ends the reading.
                let until = TIERS.iter().filter(|&&w| from > 0 && ts[from - 1].div_euclid(w) == ts[from].div_euclid(w)).map(|&w| (ts[from].div_euclid(w) + 1) * w).max();
                let calls = i - from;
                match until {
                    None => assert_eq!(calls, 0, "round {round}: nothing read where no window was cut"),
                    Some(u) => assert!(calls <= ts[from..to].iter().filter(|&&t| t < u).count() + 1, "round {round}: {calls} read"),
                }
                from = to;
            }
            j.finish(kind, usize::MAX, &mut joined);
            assert_eq!(joined, whole, "round {round}: {kind:?} every {every} ms, cut at {cuts:?}");
        }
        // Other tiers than the inputs' before, or none: not joined.
        let (ts, vs) = series(&mut rng, Kind::Gauge, 1_700_006_400_000, 10_000, 2_000);
        let mut fine = Vec::new();
        build(Kind::Gauge, &ts[..1_000], &vs[..1_000], usize::MAX, &mut b, &mut fine);
        let mut coarse = Vec::new();
        b.begin();
        for (&t, &v) in ts[1_000..].iter().zip(&vs[1_000..]) {
            b.push(t, v);
        }
        // Its five minutes' tier left out: a quarter of its samples' bytes is too few for it.
        let five = sizes(Kind::Gauge, &ts[1_000..], &vs[1_000..], &mut Builder::default())[0].1;
        b.finish(Kind::Gauge, five * KEEP_RATIO - 1, &mut coarse);
        let mut none = || None;
        j.begin();
        assert!(j.add(Kind::Gauge, &fine, &mut none).unwrap());
        assert!(!j.add(Kind::Gauge, &coarse, &mut none).unwrap(), "other tiers");
        j.begin();
        assert!(!j.add(Kind::Gauge, &[], &mut none).unwrap(), "none");
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
