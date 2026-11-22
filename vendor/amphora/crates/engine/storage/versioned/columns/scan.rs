//! Amphora: vectorized scans over a table's column copy, granule by granule, with summaries
//! (G11 step 4, CS.4; `docs/design/columnar.md` decision 1).
//!
//! A granule is one of the copy's blocks, decoded into column vectors: its size is the block size
//! [`super::Sizer`] chooses from the cost per row measured at each size tried, charged by
//! [`super::Columns::measured_scan`] per row of the table, so the granules the summaries let a scan
//! skip count for the size that let them be skipped (decision 1, part 6). No granule size is a
//! number here.
//!
//! Each granule carries its summaries: per column the least and greatest value and the null count
//! (min/max), and its first and last key and the position of its first row in the scan
//! (positional). A filter whose literal falls outside a granule's range skips the granule without
//! reading it. Filters and aggregates run over whole column vectors; grouped, one pass over the
//! group columns gives each row its group's slot in the granule, and each aggregate folds its
//! vector by slot. A column whose values mix types in a granule falls back to comparing value by
//! value, with the same answers.
//!
//! A granule whose block no change touches at the scan's snapshot is decoded once per generation
//! and kept ([`Cache`]); one with deltas, or with keys the queue holds, is decoded from its rows at
//! the snapshot each scan. So every scan reads exactly the rows at its snapshot, as
//! [`super::Columns::scan`] does.
//!
//! Morsel-parallel execution: [`Plan`] is a pipeline of the morsel executor (`vdbe/morsel.rs`,
//! lane qs6), with granules as the unit a worker takes and new groups charged to the statement's
//! shared memory budget. Every answer is the same however the granules are split among workers and
//! merged: counts and integer sums are exact, real sums exact until one rounding, and of values or
//! group keys equal under [`order`] (an integer and an equal real) the earliest row's is kept.
//!
//! Values compare as SQLite orders them under the BINARY collation: NULL first, then integers and
//! reals by value, then text bytewise, then blobs bytewise.

use super::{Block, Columns, Rows, Rowset};
use crate::error::LimboError;
use crate::numeric::Numeric;
use crate::storage::versioned::version::Ts;
use crate::sync::{Arc, Mutex};
use crate::types::{ImmutableRecordRef, ValueRef};
use crate::vdbe::morsel::{
    self, MachineCosts, MemoryBudget, MorselError, ParallelMode, Pipeline, RunStats,
};
use crate::Result;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Bound, Range};

/// One column's value in a row.
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    Null,
    Int(i64),
    Real(f64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

fn class(c: &Cell) -> u8 {
    match c {
        Cell::Null => 0,
        Cell::Int(_) | Cell::Real(_) => 1,
        Cell::Text(_) => 2,
        Cell::Blob(_) => 3,
    }
}

/// An integer against a real, exactly (SQLite's `sqlite3IntFloatCompare` gets the same answer).
/// Rounding to the nearest double is monotonic and leaves a double as it is, so where the integer's
/// nearest double differs from the real, it orders the two; where they are equal, the real is a
/// whole number and is compared as one.
fn int_real(i: i64, r: f64) -> Ordering {
    if r.is_nan() {
        return Ordering::Greater;
    }
    let near = i as f64;
    match reals(near, r) {
        Ordering::Equal => i128::from(i).cmp(&whole(r).unwrap_or_else(|| i128::from(i))),
        o => o,
    }
}

/// A whole double's value: from its bits (IEEE 754 binary64's layout, a format value), so no cast
/// rounds it. None for one with a fraction or out of i128's range.
fn whole(r: f64) -> Option<i128> {
    if r == 0.0 {
        return Some(0);
    }
    let bits = r.to_bits();
    let exp = i32::try_from(bits.checked_shr(52)? & 0x7ff).ok()?;
    let mantissa = (bits & 0x000f_ffff_ffff_ffff) | 0x0010_0000_0000_0000;
    let shift = exp.checked_sub(1075)?;
    let magnitude = if shift >= 0 {
        i128::from(mantissa).checked_shl(u32::try_from(shift).ok()?)?
    } else {
        let down = u32::try_from(shift.checked_neg()?).ok()?;
        let m = i128::from(mantissa);
        let v = m.checked_shr(down)?;
        if v.checked_shl(down)? != m {
            return None;
        }
        v
    };
    Some(if r < 0.0 {
        magnitude.checked_neg()?
    } else {
        magnitude
    })
}

/// Two reals by value (-0 equals 0); the engine stores no NaN (`NonNan`), and one would equal all.
fn reals(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

/// The order of two values: NULL, numbers by value, text, blobs.
pub fn order(a: &Cell, b: &Cell) -> Ordering {
    match (a, b) {
        (Cell::Int(x), Cell::Int(y)) => x.cmp(y),
        (Cell::Real(x), Cell::Real(y)) => reals(*x, *y),
        (Cell::Int(x), Cell::Real(y)) => int_real(*x, *y),
        (Cell::Real(x), Cell::Int(y)) => int_real(*y, *x).reverse(),
        (Cell::Text(x), Cell::Text(y)) | (Cell::Blob(x), Cell::Blob(y)) => x.cmp(y),
        _ => class(a).cmp(&class(b)),
    }
}

/// How a row's bytes become its columns.
pub trait Decode: Send + Sync {
    /// Row `row` (keyed `key`)'s columns into `out`, which comes empty.
    fn decode(&self, key: &[u8], row: &[u8], out: &mut Vec<Cell>) -> Result<()>;
}

/// Rows as SQLite record payloads, the format the engine writes a table's rows in.
pub struct Records;

impl Decode for Records {
    fn decode(&self, _key: &[u8], row: &[u8], out: &mut Vec<Cell>) -> Result<()> {
        for v in ImmutableRecordRef::from_bin_record(row).get_values()? {
            out.push(match v {
                ValueRef::Null => Cell::Null,
                ValueRef::Numeric(Numeric::Integer(i)) => Cell::Int(i),
                ValueRef::Numeric(Numeric::Float(f)) => Cell::Real(f64::from(f)),
                ValueRef::Text(t) => Cell::Text(t.as_str().as_bytes().to_vec()),
                ValueRef::Blob(b) => Cell::Blob(b.to_vec()),
            });
        }
        Ok(())
    }
}

/// A column's values in one granule.
#[derive(Clone, Debug, PartialEq)]
pub enum Data {
    Int(Vec<i64>),
    Real(Vec<f64>),
    /// Text as one buffer cut by offsets: value `i` is `bytes[offsets[i]..offsets[i + 1]]`.
    Text {
        bytes: Vec<u8>,
        offsets: Vec<usize>,
    },
    /// Values of more than one type, compared one by one.
    Mixed(Vec<Cell>),
}

/// A column vector: its values and, if any is null, which are valid (a null's slot holds the type's
/// zero, never read).
#[derive(Clone, Debug, PartialEq)]
pub struct Vector {
    pub data: Data,
    pub valid: Option<Vec<bool>>,
}

impl Vector {
    fn is_valid(&self, i: usize) -> bool {
        self.valid
            .as_ref()
            .is_none_or(|v| v.get(i).copied().unwrap_or(false))
    }

    /// Value `i` as a cell (NULL past the end).
    pub fn cell(&self, i: usize) -> Cell {
        if !self.is_valid(i) {
            return Cell::Null;
        }
        match &self.data {
            Data::Int(v) => v.get(i).map_or(Cell::Null, |x| Cell::Int(*x)),
            Data::Real(v) => v.get(i).map_or(Cell::Null, |x| Cell::Real(*x)),
            Data::Text { bytes, offsets } => {
                match (
                    offsets.get(i),
                    i.checked_add(1).and_then(|n| offsets.get(n)),
                ) {
                    (Some(s), Some(e)) => bytes
                        .get(*s..*e)
                        .map_or(Cell::Null, |b| Cell::Text(b.to_vec())),
                    _ => Cell::Null,
                }
            }
            Data::Mixed(v) => v.get(i).cloned().unwrap_or(Cell::Null),
        }
    }

    /// The vector of `cells`, typed when every value that isn't null has one type.
    fn of(cells: Vec<Cell>) -> Self {
        let mut kind: Option<u8> = None;
        let mut mixed = false;
        let mut nulls = false;
        for c in &cells {
            let k = match c {
                Cell::Null => {
                    nulls = true;
                    continue;
                }
                Cell::Int(_) => 0,
                Cell::Real(_) => 1,
                Cell::Text(_) => 2,
                Cell::Blob(_) => 3,
            };
            if kind.is_some_and(|was| was != k) {
                mixed = true;
            }
            kind = Some(k);
        }
        let valid = nulls.then(|| cells.iter().map(|c| *c != Cell::Null).collect());
        let data = match kind {
            _ if mixed => Data::Mixed(cells),
            None | Some(0) => Data::Int(
                cells
                    .iter()
                    .map(|c| if let Cell::Int(x) = c { *x } else { 0 })
                    .collect(),
            ),
            Some(1) => Data::Real(
                cells
                    .iter()
                    .map(|c| if let Cell::Real(x) = c { *x } else { 0.0 })
                    .collect(),
            ),
            Some(2) => {
                let mut bytes = Vec::new();
                let mut offsets = Vec::with_capacity(cells.len().saturating_add(1));
                offsets.push(0);
                for c in &cells {
                    if let Cell::Text(t) = c {
                        bytes.extend_from_slice(t);
                    }
                    offsets.push(bytes.len());
                }
                Data::Text { bytes, offsets }
            }
            Some(_) => Data::Mixed(cells),
        };
        Vector { data, valid }
    }
}

/// A column's min/max summary in one granule: its least and greatest value that isn't null (None
/// when every value is null), and how many are null.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub min: Option<Cell>,
    pub max: Option<Cell>,
    pub nulls: usize,
}

/// One block of the copy, decoded: its column vectors and their summaries, and its first and last
/// key (with [`Placed::at`], its positional summary).
#[derive(Clone, Debug, PartialEq)]
pub struct Granule {
    pub first: Vec<u8>,
    pub last: Vec<u8>,
    pub rows: usize,
    /// Each row's key, in order: what a key range's boundary granules are filtered by.
    pub keys: KeyColumn,
    pub columns: Vec<Vector>,
    pub summaries: Vec<Summary>,
}

impl Granule {
    /// The granule of `rows` (in key order).
    pub fn build(rows: &[(Vec<u8>, Vec<u8>)], dec: &dyn Decode) -> Result<Self> {
        let mut decoded: Vec<Vec<Cell>> = Vec::new();
        let mut width = 0usize;
        for (k, v) in rows {
            let mut cells = Vec::new();
            dec.decode(k, v, &mut cells)?;
            width = width.max(cells.len());
            decoded.push(cells);
        }
        let mut columns = Vec::with_capacity(width);
        let mut summaries = Vec::with_capacity(width);
        for c in 0..width {
            let cells: Vec<Cell> = decoded
                .iter_mut()
                .map(|r| {
                    r.get_mut(c)
                        .map_or(Cell::Null, |x| std::mem::replace(x, Cell::Null))
                })
                .collect();
            let mut s = Summary {
                min: None,
                max: None,
                nulls: 0,
            };
            for cell in &cells {
                if *cell == Cell::Null {
                    s.nulls = s.nulls.saturating_add(1);
                    continue;
                }
                if s.min
                    .as_ref()
                    .is_none_or(|m| order(cell, m) == Ordering::Less)
                {
                    s.min = Some(cell.clone());
                }
                if s.max
                    .as_ref()
                    .is_none_or(|m| order(cell, m) == Ordering::Greater)
                {
                    s.max = Some(cell.clone());
                }
            }
            columns.push(Vector::of(cells));
            summaries.push(s);
        }
        let mut keys = KeyColumn::default();
        for (k, _) in rows {
            keys.push(k);
        }
        Ok(Granule {
            first: rows.first().map(|(k, _)| k.clone()).unwrap_or_default(),
            last: rows.last().map(|(k, _)| k.clone()).unwrap_or_default(),
            rows: rows.len(),
            keys,
            columns,
            summaries,
        })
    }

    /// Column `c`'s value in row `i` (NULL for a column the granule's rows don't reach).
    fn cell(&self, c: usize, i: usize) -> Cell {
        self.columns.get(c).map_or(Cell::Null, |v| v.cell(i))
    }

    /// The bytes it keeps resident, about: its vectors' values and keys.
    pub fn resident(&self) -> usize {
        let cell = |c: &Cell| match c {
            Cell::Text(b) | Cell::Blob(b) => b.len().saturating_add(std::mem::size_of::<Cell>()),
            _ => std::mem::size_of::<Cell>(),
        };
        self.columns.iter().fold(
            self.first
                .len()
                .saturating_add(self.last.len())
                .saturating_add(self.keys.resident()),
            |n, v| {
                let data = match &v.data {
                    Data::Int(x) => x.len().saturating_mul(8),
                    Data::Real(x) => x.len().saturating_mul(8),
                    Data::Text { bytes, offsets } => {
                        bytes.len().saturating_add(offsets.len().saturating_mul(8))
                    }
                    Data::Mixed(x) => x.iter().fold(0usize, |n, c| n.saturating_add(cell(c))),
                };
                n.saturating_add(data)
                    .saturating_add(v.valid.as_ref().map_or(0, Vec::len))
            },
        )
    }
}

/// A granule's keys as one buffer cut by offsets: key `i` is `bytes[ends[i - 1]..ends[i]]`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeyColumn {
    bytes: Vec<u8>,
    ends: Vec<usize>,
}

impl KeyColumn {
    fn push(&mut self, k: &[u8]) {
        self.bytes.extend_from_slice(k);
        self.ends.push(self.bytes.len());
    }

    /// Key `i`, or None past the end.
    pub fn get(&self, i: usize) -> Option<&[u8]> {
        let start = match i.checked_sub(1) {
            None => 0,
            Some(before) => *self.ends.get(before)?,
        };
        self.bytes.get(start..*self.ends.get(i)?)
    }

    fn resident(&self) -> usize {
        self.bytes
            .len()
            .saturating_add(self.ends.len().saturating_mul(std::mem::size_of::<usize>()))
    }
}

/// The keys a statement reads, bytewise as the copy orders them: a whole-table scan is unbounded
/// both ways, a wide-range one bounded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRange {
    pub lo: Bound<Vec<u8>>,
    pub hi: Bound<Vec<u8>>,
}

impl Default for KeyRange {
    fn default() -> Self {
        KeyRange {
            lo: Bound::Unbounded,
            hi: Bound::Unbounded,
        }
    }
}

impl KeyRange {
    pub fn contains(&self, k: &[u8]) -> bool {
        let above = match &self.lo {
            Bound::Included(l) => k >= l.as_slice(),
            Bound::Excluded(l) => k > l.as_slice(),
            Bound::Unbounded => true,
        };
        let below = match &self.hi {
            Bound::Included(h) => k <= h.as_slice(),
            Bound::Excluded(h) => k < h.as_slice(),
            Bound::Unbounded => true,
        };
        above && below
    }

    fn is_full(&self) -> bool {
        matches!((&self.lo, &self.hi), (Bound::Unbounded, Bound::Unbounded))
    }

    /// Whether no key after `after` (None: from the start) and up to `upto` (None: to the end)
    /// is in the range: a block whose keys all fall there is skipped before it is decoded.
    fn misses(&self, after: Option<&[u8]>, upto: Option<&[u8]>) -> bool {
        let below = match (&self.lo, upto) {
            (Bound::Included(l), Some(u)) => u < l.as_slice(),
            (Bound::Excluded(l), Some(u)) => u <= l.as_slice(),
            _ => false,
        };
        let above = match (&self.hi, after) {
            (Bound::Included(h) | Bound::Excluded(h), Some(a)) => a >= h.as_slice(),
            _ => false,
        };
        below || above
    }

    /// Whether a granule from `first` to `last` holds no key in the range.
    fn excludes(&self, first: &[u8], last: &[u8]) -> bool {
        let below = match &self.lo {
            Bound::Included(l) => last < l.as_slice(),
            Bound::Excluded(l) => last <= l.as_slice(),
            Bound::Unbounded => false,
        };
        let above = match &self.hi {
            Bound::Included(h) => first > h.as_slice(),
            Bound::Excluded(h) => first >= h.as_slice(),
            Bound::Unbounded => false,
        };
        below || above
    }

    /// Whether every key from `first` to `last` is in the range.
    fn covers(&self, first: &[u8], last: &[u8]) -> bool {
        self.contains(first) && self.contains(last)
    }
}

/// A granule in a scan: where its first row is among the scan's rows, and the copy's block it is
/// (None: cut from the rows, when no generation answered the scan).
#[derive(Clone, Debug)]
pub struct Placed {
    pub at: usize,
    pub block: Option<usize>,
    pub granule: Arc<Granule>,
}

/// A comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Op {
    fn holds(self, o: Ordering) -> bool {
        match self {
            Op::Eq => o == Ordering::Equal,
            Op::Ne => o != Ordering::Equal,
            Op::Lt => o == Ordering::Less,
            Op::Le => o != Ordering::Greater,
            Op::Gt => o == Ordering::Greater,
            Op::Ge => o != Ordering::Less,
        }
    }
}

/// A filter on one column. A comparison with NULL on either side holds for no row, as in SQL.
#[derive(Clone, Debug, PartialEq)]
pub enum Filter {
    Cmp { col: usize, op: Op, lit: Cell },
    IsNull(usize),
    NotNull(usize),
}

impl Filter {
    /// Whether it holds for a row's cells (the row-at-a-time rule the vectors must agree with).
    pub fn matches(&self, row: &[Cell]) -> bool {
        let at = |c: usize| row.get(c).unwrap_or(&Cell::Null);
        match self {
            Filter::Cmp { col, op, lit } => {
                let v = at(*col);
                *v != Cell::Null && *lit != Cell::Null && op.holds(order(v, lit))
            }
            Filter::IsNull(c) => *at(*c) == Cell::Null,
            Filter::NotNull(c) => *at(*c) != Cell::Null,
        }
    }

    /// Whether the granule's summaries show no row of it can match: the granule is skipped.
    fn excludes(&self, g: &Granule) -> bool {
        let summary = |c: usize| g.summaries.get(c);
        match self {
            Filter::Cmp { col, op, lit } => {
                if *lit == Cell::Null {
                    return true;
                }
                let Some(Summary {
                    min: Some(min),
                    max: Some(max),
                    ..
                }) = summary(*col)
                else {
                    // Every value null, or a column no row reaches.
                    return true;
                };
                let (lo, hi) = (order(min, lit), order(max, lit));
                match op {
                    Op::Eq => lo == Ordering::Greater || hi == Ordering::Less,
                    Op::Ne => lo == Ordering::Equal && hi == Ordering::Equal,
                    Op::Lt => lo != Ordering::Less,
                    Op::Le => lo == Ordering::Greater,
                    Op::Gt => hi != Ordering::Greater,
                    Op::Ge => hi == Ordering::Less,
                }
            }
            Filter::IsNull(c) => summary(*c).is_some_and(|s| s.nulls == 0),
            Filter::NotNull(c) => summary(*c).is_none_or(|s| s.nulls == g.rows),
        }
    }

    /// Clears `sel` where the filter fails, over the granule's whole column vector.
    fn apply(&self, g: &Granule, sel: &mut [bool]) {
        let (col, test): (usize, Option<(Op, &Cell)>) = match self {
            Filter::Cmp { col, op, lit } => (*col, Some((*op, lit))),
            Filter::IsNull(c) | Filter::NotNull(c) => (*c, None),
        };
        let Some(v) = g.columns.get(col) else {
            // No row reaches the column: every value is NULL.
            let keep = matches!(self, Filter::IsNull(_));
            sel.iter_mut().for_each(|s| *s &= keep);
            return;
        };
        let Some((op, lit)) = test else {
            let want = matches!(self, Filter::IsNull(_));
            match &v.valid {
                Some(valid) => sel
                    .iter_mut()
                    .zip(valid)
                    .for_each(|(s, ok)| *s &= *ok != want),
                None => sel.iter_mut().for_each(|s| *s &= !want),
            }
            return;
        };
        if *lit == Cell::Null {
            sel.iter_mut().for_each(|s| *s = false);
            return;
        }
        match (&v.data, lit) {
            (Data::Int(xs), Cell::Int(y)) => {
                let y = *y;
                sel.iter_mut()
                    .zip(xs)
                    .for_each(|(s, x)| *s &= op.holds(x.cmp(&y)));
            }
            (Data::Real(xs), Cell::Real(y)) => {
                let y = *y;
                sel.iter_mut()
                    .zip(xs)
                    .for_each(|(s, x)| *s &= op.holds(reals(*x, y)));
            }
            (Data::Int(xs), Cell::Real(y)) => sel
                .iter_mut()
                .zip(xs)
                .for_each(|(s, x)| *s &= op.holds(int_real(*x, *y))),
            (Data::Real(xs), Cell::Int(y)) => sel
                .iter_mut()
                .zip(xs)
                .for_each(|(s, x)| *s &= op.holds(int_real(*y, *x).reverse())),
            (Data::Text { bytes, offsets }, Cell::Text(y)) => {
                sel.iter_mut().zip(offsets.windows(2)).for_each(|(s, w)| {
                    let x = match w {
                        [a, b] => bytes.get(*a..*b).unwrap_or(&[]),
                        _ => &[],
                    };
                    *s &= op.holds(x.cmp(y.as_slice()));
                });
            }
            (Data::Mixed(xs), lit) => sel
                .iter_mut()
                .zip(xs)
                .for_each(|(s, x)| *s &= *x != Cell::Null && op.holds(order(x, lit))),
            // Typed values of another class than the literal's: one answer for all of them.
            (Data::Int(_) | Data::Real(_) | Data::Text { .. }, lit) => {
                let one = match &v.data {
                    Data::Text { .. } => Cell::Text(Vec::new()),
                    _ => Cell::Int(0),
                };
                let holds = op.holds(class(&one).cmp(&class(lit)));
                sel.iter_mut().for_each(|s| *s &= holds);
            }
        }
        if let Some(valid) = &v.valid {
            sel.iter_mut().zip(valid).for_each(|(s, ok)| *s &= *ok);
        }
    }
}

/// An aggregate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agg {
    /// COUNT(*).
    Count,
    /// COUNT(col): the values that aren't null.
    CountOf(usize),
    /// SUM(col) over its numbers: an integer while every value is one, else a real; NULL when it
    /// has none. Text and blobs are not summed.
    Sum(usize),
    Min(usize),
    Max(usize),
}

impl Agg {
    fn col(self) -> Option<usize> {
        match self {
            Agg::Count => None,
            Agg::CountOf(c) | Agg::Sum(c) | Agg::Min(c) | Agg::Max(c) => Some(c),
        }
    }
}

/// A real sum kept exactly, as non-overlapping parts (Shewchuk 1997, "Adaptive Precision
/// Floating-Point Arithmetic"): the same however its values are split and merged. Its parts are
/// bounded by binary64's exponent range over its precision, a format value.
#[derive(Clone, Debug, Default, PartialEq)]
struct ExactSum {
    parts: Vec<f64>,
    /// The plain sum of what isn't finite, or of parts that overflowed: what it then is.
    special: Option<f64>,
}

impl ExactSum {
    fn add(&mut self, mut x: f64) {
        if !x.is_finite() {
            self.special = Some(self.special.unwrap_or(0.0) + x);
            return;
        }
        let mut kept = Vec::with_capacity(self.parts.len().saturating_add(1));
        for mut y in self.parts.drain(..) {
            if x.abs() < y.abs() {
                std::mem::swap(&mut x, &mut y);
            }
            let hi = x + y;
            if !hi.is_finite() {
                self.special = Some(self.special.unwrap_or(0.0) + hi);
                x = 0.0;
                continue;
            }
            let lo = y - (hi - x);
            if lo != 0.0 {
                kept.push(lo);
            }
            x = hi;
        }
        kept.push(x);
        self.parts = kept;
    }

    fn merge(&mut self, other: &ExactSum) {
        for p in &other.parts {
            self.add(*p);
        }
        if let Some(s) = other.special {
            self.special = Some(self.special.unwrap_or(0.0) + s);
        }
    }

    /// The sum, rounded once (Python's `math.fsum`'s last step).
    fn value(&self) -> f64 {
        if let Some(s) = self.special {
            return s;
        }
        let p = &self.parts;
        let mut n = p.len();
        let mut hi = 0.0f64;
        let mut lo = 0.0f64;
        if n > 0 {
            n = n.saturating_sub(1);
            hi = p.get(n).copied().unwrap_or(0.0);
            while n > 0 {
                let x = hi;
                n = n.saturating_sub(1);
                let y = p.get(n).copied().unwrap_or(0.0);
                hi = x + y;
                let yr = hi - x;
                lo = y - yr;
                if lo != 0.0 {
                    break;
                }
            }
            if n > 0 {
                let next = p.get(n.saturating_sub(1)).copied().unwrap_or(0.0);
                if (lo < 0.0 && next < 0.0) || (lo > 0.0 && next > 0.0) {
                    let y = lo * 2.0;
                    let x = hi + y;
                    if y == x - hi {
                        hi = x;
                    }
                }
            }
        }
        hi
    }
}

/// One aggregate's running state.
#[derive(Clone, Debug, PartialEq)]
enum Acc {
    Count(u64),
    Sum {
        ints: i128,
        reals: ExactSum,
        any_real: bool,
        numbers: u64,
    },
    /// The least value and its row's position: ties go to the earliest row, whatever order the
    /// partials merge in.
    Min(Option<(Cell, u64)>),
    Max(Option<(Cell, u64)>),
}

fn overflow() -> LimboError {
    LimboError::IntegerOverflow
}

impl Acc {
    fn new(a: Agg) -> Self {
        match a {
            Agg::Count | Agg::CountOf(_) => Acc::Count(0),
            Agg::Sum(_) => Acc::Sum {
                ints: 0,
                reals: ExactSum::default(),
                any_real: false,
                numbers: 0,
            },
            Agg::Min(_) => Acc::Min(None),
            Agg::Max(_) => Acc::Max(None),
        }
    }

    /// Folds one value, of the row at position `pos` (for COUNT(*), any non-null cell).
    fn fold(&mut self, c: &Cell, pos: u64) -> Result<()> {
        if *c == Cell::Null {
            return Ok(());
        }
        match self {
            Acc::Count(n) => *n = n.checked_add(1).ok_or_else(overflow)?,
            Acc::Sum {
                ints,
                reals,
                any_real,
                numbers,
            } => match c {
                Cell::Int(x) => {
                    *ints = ints.checked_add(i128::from(*x)).ok_or_else(overflow)?;
                    *numbers = numbers.checked_add(1).ok_or_else(overflow)?;
                }
                Cell::Real(x) => {
                    reals.add(*x);
                    *any_real = true;
                    *numbers = numbers.checked_add(1).ok_or_else(overflow)?;
                }
                _ => {}
            },
            Acc::Min(m) => {
                if m.as_ref()
                    .is_none_or(|(m, at)| order(c, m).then(pos.cmp(at)) == Ordering::Less)
                {
                    *m = Some((c.clone(), pos));
                }
            }
            Acc::Max(m) => {
                if m.as_ref()
                    .is_none_or(|(m, at)| order(c, m).then(at.cmp(&pos)) == Ordering::Greater)
                {
                    *m = Some((c.clone(), pos));
                }
            }
        }
        Ok(())
    }

    /// Folds another partial's state for the same aggregate.
    fn merge(&mut self, later: &Acc) -> Result<()> {
        let later_kind = matches!(later, Acc::Min(_));
        match (self, later) {
            (Acc::Count(a), Acc::Count(b)) => *a = a.checked_add(*b).ok_or_else(overflow)?,
            (
                Acc::Sum {
                    ints,
                    reals,
                    any_real,
                    numbers,
                },
                Acc::Sum {
                    ints: i2,
                    reals: r2,
                    any_real: a2,
                    numbers: n2,
                },
            ) => {
                *ints = ints.checked_add(*i2).ok_or_else(overflow)?;
                reals.merge(r2);
                *any_real |= *a2;
                *numbers = numbers.checked_add(*n2).ok_or_else(overflow)?;
            }
            (me, Acc::Min(later) | Acc::Max(later))
                if matches!(
                    (&*me, later_kind),
                    (Acc::Min(_), true) | (Acc::Max(_), false)
                ) =>
            {
                if let Some((c, pos)) = later {
                    me.fold(c, *pos)?;
                }
            }
            _ => {
                return Err(LimboError::InternalError(
                    "column scan: aggregates of different kinds merged".into(),
                ));
            }
        }
        Ok(())
    }

    fn finish(&self) -> Result<Cell> {
        Ok(match self {
            Acc::Count(n) => Cell::Int(i64::try_from(*n).map_err(|_| overflow())?),
            Acc::Sum {
                ints,
                reals,
                any_real,
                numbers,
            } => {
                if *numbers == 0 {
                    Cell::Null
                } else if *any_real {
                    let mut all = reals.clone();
                    // The integers' total joins the reals', in 32-bit parts, each exact as a double.
                    let magnitude = ints.unsigned_abs();
                    let mut scale = if *ints < 0 { -1.0f64 } else { 1.0 };
                    for k in 0..4u32 {
                        let part = magnitude
                            .checked_shr(k.saturating_mul(32))
                            .map_or(0, |v| u32::try_from(v & 0xffff_ffff).unwrap_or(0));
                        all.add(f64::from(part) * scale);
                        // 2^32: a format value, exact as a double.
                        scale *= 4_294_967_296.0;
                    }
                    Cell::Real(all.value())
                } else {
                    Cell::Int(i64::try_from(*ints).map_err(|_| overflow())?)
                }
            }
            Acc::Min(m) | Acc::Max(m) => m.as_ref().map_or(Cell::Null, |(c, _)| c.clone()),
        })
    }
}

/// A group's key: its cells, ordered as [`order`] orders them, column by column.
#[derive(Clone, Debug)]
struct Key(Vec<Cell>);

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Key {}
impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        for (a, b) in self.0.iter().zip(&other.0) {
            let o = order(a, b);
            if o != Ordering::Equal {
                return o;
            }
        }
        self.0.len().cmp(&other.0.len())
    }
}

/// An analytic statement over one table: rows passing every filter, grouped by the `group`
/// columns (none: one group), each group's aggregates.
#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    pub filters: Vec<Filter>,
    pub group: Vec<usize>,
    pub aggs: Vec<Agg>,
    /// The bytes the groups may take: the statement's share of the memory budget (CLAUDE.md §2).
    /// A group past it is refused.
    pub budget: usize,
    /// The keys it reads.
    pub keys: KeyRange,
}

/// What a scan counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Granules read.
    pub granules: u64,
    /// Granules their summaries skipped.
    pub skipped: u64,
    /// Rows in the granules read.
    pub rows: u64,
    /// Rows that passed the filters.
    pub selected: u64,
}

impl Counts {
    fn merge(&mut self, o: &Counts) {
        self.granules = self.granules.saturating_add(o.granules);
        self.skipped = self.skipped.saturating_add(o.skipped);
        self.rows = self.rows.saturating_add(o.rows);
        self.selected = self.selected.saturating_add(o.selected);
    }
}

/// A worker's state: its groups, their bytes, and its counts.
#[derive(Clone, Debug, Default)]
pub struct Partial {
    /// Each group's accumulators and the position of the row its key came from: of keys equal
    /// under [`order`] (an integer and an equal real), the earliest row's is kept.
    groups: BTreeMap<Key, (u64, Vec<Acc>)>,
    bytes: usize,
    pub counts: Counts,
}

fn key_bytes(k: &Key, aggs: usize) -> usize {
    k.0.iter()
        .fold(aggs.saturating_mul(std::mem::size_of::<Acc>()), |n, c| {
            let extra = match c {
                Cell::Text(b) | Cell::Blob(b) => b.len(),
                _ => 0,
            };
            n.saturating_add(std::mem::size_of::<Cell>())
                .saturating_add(extra)
        })
}

/// The statement row by row over `rows` (each key and its cells, in key order): the reference
/// the vectors are checked against.
pub fn evaluate_rows(q: &Query, rows: &[(Vec<u8>, Vec<Cell>)]) -> Result<Vec<Vec<Cell>>> {
    let mut p = Partial::default();
    let mut charge = within(q);
    for (i, (k, r)) in rows.iter().enumerate() {
        if !q.keys.contains(k) || !q.filters.iter().all(|f| f.matches(r)) {
            continue;
        }
        let pos = u64::try_from(i).unwrap_or(u64::MAX);
        let key = Key(q
            .group
            .iter()
            .map(|c| r.get(*c).cloned().unwrap_or(Cell::Null))
            .collect());
        let accs = group(q, &mut p, key, pos, &mut charge)?;
        for (acc, a) in accs.iter_mut().zip(&q.aggs) {
            let c = match a.col() {
                None => Cell::Int(1),
                Some(c) => r.get(c).cloned().unwrap_or(Cell::Null),
            };
            acc.fold(&c, pos)?;
        }
    }
    finish(q, p)
}

/// What a new group's bytes are charged to: called with its bytes and the partial's total after
/// it; an error refuses the group.
type Charge<'c> = dyn FnMut(usize, usize) -> Result<()> + 'c;

/// Charges against the statement's own budget, [`Query::budget`].
fn within(q: &Query) -> impl FnMut(usize, usize) -> Result<()> + '_ {
    move |_, total| {
        if total <= q.budget {
            Ok(())
        } else {
            Err(LimboError::OutOfMemory)
        }
    }
}

/// The group `key`'s accumulators (its key from the row at `pos`), made and charged when new.
fn group<'p>(
    q: &Query,
    p: &'p mut Partial,
    key: Key,
    pos: u64,
    charge: &mut Charge<'_>,
) -> Result<&'p mut Vec<Acc>> {
    match p.groups.get(&key).map(|(at, _)| *at) {
        Some(at) if pos >= at => {}
        Some(_) => {
            // An earlier row's equal key: it becomes the group's key.
            if let Some((_, (_, accs))) = p.groups.remove_entry(&key) {
                p.groups.insert(key.clone(), (pos, accs));
            }
        }
        None => {
            let bytes = key_bytes(&key, q.aggs.len());
            let total = p.bytes.checked_add(bytes).ok_or(LimboError::OutOfMemory)?;
            charge(bytes, total)?;
            p.bytes = total;
            let accs = q.aggs.iter().map(|a| Acc::new(*a)).collect();
            p.groups.insert(key.clone(), (pos, accs));
        }
    }
    p.groups
        .get_mut(&key)
        .map(|(_, accs)| accs)
        .ok_or_else(|| LimboError::InternalError("column scan: a group just made is gone".into()))
}

fn finish(q: &Query, mut p: Partial) -> Result<Vec<Vec<Cell>>> {
    if q.group.is_empty() && p.groups.is_empty() {
        p.groups.insert(
            Key(Vec::new()),
            (0, q.aggs.iter().map(|a| Acc::new(*a)).collect()),
        );
    }
    let mut out = Vec::with_capacity(p.groups.len());
    for (k, (_, accs)) in p.groups {
        let mut row = k.0;
        for a in &accs {
            row.push(a.finish()?);
        }
        out.push(row);
    }
    Ok(out)
}

/// A statement over a scan's granules, run granule range by granule range into partial states
/// that merge: the morsel executor's pipeline shape, with granules as its unit.
pub struct Plan<'a> {
    pub granules: Vec<Placed>,
    pub query: &'a Query,
}

impl Plan<'_> {
    /// How many granules it reads: the units a worker takes.
    pub fn rows(&self) -> usize {
        self.granules.len()
    }

    pub fn empty(&self) -> Partial {
        Partial::default()
    }

    /// Folds granules `range` into `p`, its groups charged to [`Query::budget`].
    pub fn run(&self, range: Range<usize>, p: &mut Partial) -> Result<()> {
        self.fold(range, p, &mut within(self.query))
    }

    fn fold(&self, range: Range<usize>, p: &mut Partial, charge: &mut Charge<'_>) -> Result<()> {
        let q = self.query;
        let Some(granules) = self.granules.get(range) else {
            return Err(LimboError::InternalError(
                "column scan: a granule range past the scan".into(),
            ));
        };
        let mut sel = Vec::new();
        for placed in granules {
            let g = &*placed.granule;
            if q.keys.excludes(&g.first, &g.last) || q.filters.iter().any(|f| f.excludes(g)) {
                p.counts.skipped = p.counts.skipped.saturating_add(1);
                continue;
            }
            p.counts.granules = p.counts.granules.saturating_add(1);
            p.counts.rows = p
                .counts
                .rows
                .saturating_add(u64::try_from(g.rows).unwrap_or(u64::MAX));
            sel.clear();
            sel.resize(g.rows, true);
            if !q.keys.covers(&g.first, &g.last) {
                for (i, s) in sel.iter_mut().enumerate() {
                    *s = g.keys.get(i).is_some_and(|k| q.keys.contains(k));
                }
            }
            for f in &q.filters {
                f.apply(g, &mut sel);
            }
            let selected = sel.iter().filter(|s| **s).count();
            p.counts.selected = p
                .counts
                .selected
                .saturating_add(u64::try_from(selected).unwrap_or(u64::MAX));
            let at = u64::try_from(placed.at).unwrap_or(u64::MAX);
            if q.group.is_empty() {
                let accs = group(q, p, Key(Vec::new()), at, charge)?;
                for (acc, a) in accs.iter_mut().zip(&q.aggs) {
                    fold_vector(acc, *a, g, &sel, selected, at)?;
                }
            } else {
                fold_groups(q, g, &sel, at, p, charge)?;
            }
        }
        Ok(())
    }

    /// Folds `later`, another partial's state, into `into`, new groups charged to
    /// [`Query::budget`]. Answers don't depend on which granules each partial ran.
    pub fn merge(&self, into: &mut Partial, later: Partial) -> Result<()> {
        self.merge_with(into, later, &mut within(self.query))
    }

    fn merge_with(
        &self,
        into: &mut Partial,
        later: Partial,
        charge: &mut Charge<'_>,
    ) -> Result<()> {
        into.counts.merge(&later.counts);
        for (k, (pos, accs)) in later.groups {
            let mine = group(self.query, into, k, pos, charge)?;
            for (a, b) in mine.iter_mut().zip(&accs) {
                a.merge(b)?;
            }
        }
        Ok(())
    }

    /// The statement's rows: each group's key columns, then its aggregates, groups in key order.
    pub fn finish(&self, p: Partial) -> Result<Vec<Vec<Cell>>> {
        finish(self.query, p)
    }

    /// Runs the statement on as many cores as the morsel executor finds pay, from its pilot's cost
    /// per granule and the machine's measured costs (`mode` off: the caller's thread alone).
    pub fn run_on_cores(
        &self,
        mode: ParallelMode,
        costs: &mut MachineCosts,
        budget: &MemoryBudget,
    ) -> std::result::Result<(Vec<Vec<Cell>>, Counts, RunStats), MorselError> {
        let (p, stats) = morsel::execute(self, mode, costs, budget)?;
        let counts = p.counts;
        let rows = self.finish(p).map_err(morsel_error)?;
        Ok((rows, counts, stats))
    }

    /// Runs every granule on the caller's thread: the serial degree.
    pub fn run_all(&self) -> Result<(Vec<Vec<Cell>>, Counts)> {
        let mut p = self.empty();
        self.run(0..self.rows(), &mut p)?;
        let counts = p.counts;
        Ok((self.finish(p)?, counts))
    }
}

/// The morsel executor's pipeline (`vdbe/morsel.rs`): granules are the unit its workers take,
/// and the statement's memory budget it shares among them is what new groups are charged to.
impl Pipeline for Plan<'_> {
    type Partial = Partial;

    fn rows(&self) -> usize {
        self.granules.len()
    }

    fn empty(&self) -> Partial {
        Partial::default()
    }

    fn run(
        &self,
        rows: Range<usize>,
        partial: &mut Partial,
        budget: &MemoryBudget,
    ) -> std::result::Result<(), MorselError> {
        let mut refused = None;
        let done = self.fold(rows, partial, &mut |bytes, _| {
            budget.charge(bytes).map_err(|e| {
                refused = Some(e);
                LimboError::OutOfMemory
            })
        });
        morsel_result(done, refused)
    }

    fn merge(
        &self,
        into: &mut Partial,
        from: Partial,
        _budget: &MemoryBudget,
    ) -> std::result::Result<(), MorselError> {
        // A group's bytes were charged by the worker that made it.
        morsel_result(self.merge_with(into, from, &mut |_, _| Ok(())), None)
    }
}

/// A scan's error as the executor's: the budget's own refusal when it refused, an integer past
/// its type as such, and a granule range past the scan (the only other) as an overflow of it.
fn morsel_result(
    done: Result<()>,
    refused: Option<MorselError>,
) -> std::result::Result<(), MorselError> {
    match (done, refused) {
        (Ok(()), _) => Ok(()),
        (Err(_), Some(e)) => Err(e),
        (Err(e), None) => Err(morsel_error(e)),
    }
}

fn morsel_error(e: LimboError) -> MorselError {
    match e {
        LimboError::IntegerOverflow => MorselError::IntegerOverflow,
        _ => MorselError::Overflow {
            what: "a column scan's granule range",
        },
    }
}

/// Folds a granule's selected rows into their groups (T-CS.4.1). One pass over the group columns
/// gives each selected row its group's slot in the granule (keys equal under [`order`] share one,
/// the earliest row's key kept); each aggregate then folds its whole column vector by slot, and
/// each group is looked up in the partial once a granule, from its earliest row. The slots are
/// bounded by the granule's rows. `at` is the position of the granule's first row.
fn fold_groups(
    q: &Query,
    g: &Granule,
    sel: &[bool],
    at: u64,
    p: &mut Partial,
    charge: &mut Charge<'_>,
) -> Result<()> {
    let pos = |i: usize| at.saturating_add(u64::try_from(i).unwrap_or(u64::MAX));
    let mut local: BTreeMap<Key, usize> = BTreeMap::new();
    // Each slot's earliest row.
    let mut first: Vec<usize> = Vec::new();
    let mut slots: Vec<Option<usize>> = vec![None; sel.len()];
    for ((i, s), slot) in sel.iter().enumerate().zip(slots.iter_mut()) {
        if !*s {
            continue;
        }
        let next = first.len();
        let at_slot = *local
            .entry(Key(q.group.iter().map(|c| g.cell(*c, i)).collect()))
            .or_insert(next);
        if at_slot == next {
            first.push(i);
        }
        *slot = Some(at_slot);
    }
    let mut accs: Vec<Vec<Acc>> = first
        .iter()
        .map(|_| q.aggs.iter().map(|a| Acc::new(*a)).collect())
        .collect();
    for (j, a) in q.aggs.iter().enumerate() {
        fold_slots(&mut accs, j, *a, g, &slots, at)?;
    }
    let mut keys: Vec<Option<Key>> = vec![None; first.len()];
    for (k, s) in local {
        if let Some(x) = keys.get_mut(s) {
            *x = Some(k);
        }
    }
    for ((key, row), mine) in keys.into_iter().zip(&first).zip(&accs) {
        let Some(key) = key else {
            return Err(LimboError::InternalError(
                "column scan: a granule's group without its key".into(),
            ));
        };
        let theirs = group(q, p, key, pos(*row), charge)?;
        for (a, b) in theirs.iter_mut().zip(mine) {
            a.merge(b)?;
        }
    }
    Ok(())
}

/// Each slot's first row holding its least (or greatest) value, over a typed vector, compared in
/// its own type; None for a vector whose values mix types (those are compared one by one).
/// `rows` gives each selected row with a value, and its slot (below `slots`).
fn extremes(
    v: &Vector,
    rows: impl Iterator<Item = (usize, usize)>,
    slots: usize,
    want_min: bool,
) -> Option<Vec<Option<usize>>> {
    match &v.data {
        Data::Int(xs) => Some(pick(rows, slots, want_min, |i, j| {
            match (xs.get(i), xs.get(j)) {
                (Some(a), Some(b)) => a.cmp(b),
                _ => Ordering::Equal,
            }
        })),
        Data::Real(xs) => Some(pick(rows, slots, want_min, |i, j| {
            match (xs.get(i), xs.get(j)) {
                (Some(a), Some(b)) => reals(*a, *b),
                _ => Ordering::Equal,
            }
        })),
        Data::Text { bytes, offsets } => {
            let at = |i: usize| match (
                offsets.get(i),
                i.checked_add(1).and_then(|n| offsets.get(n)),
            ) {
                (Some(a), Some(b)) => bytes.get(*a..*b).unwrap_or(&[]),
                _ => &[],
            };
            Some(pick(rows, slots, want_min, |i, j| at(i).cmp(at(j))))
        }
        Data::Mixed(_) => None,
    }
}

/// [`extremes`] by `cmp`, row against row: ties keep the earlier row.
fn pick(
    rows: impl Iterator<Item = (usize, usize)>,
    slots: usize,
    want_min: bool,
    cmp: impl Fn(usize, usize) -> Ordering,
) -> Vec<Option<usize>> {
    let want = if want_min {
        Ordering::Less
    } else {
        Ordering::Greater
    };
    let mut best: Vec<Option<usize>> = vec![None; slots];
    for (i, s) in rows {
        if let Some(b) = best.get_mut(s) {
            if (*b).is_none_or(|j| cmp(i, j) == want) {
                *b = Some(i);
            }
        }
    }
    best
}

/// Slot `s`'s accumulator for aggregate `j`.
fn slot(accs: &mut [Vec<Acc>], s: usize, j: usize) -> Option<&mut Acc> {
    accs.get_mut(s).and_then(|x| x.get_mut(j))
}

/// Aggregate `j` of each slot's accumulators, folded over its column's whole vector: `slots` holds
/// each row's slot (None: not selected). `at` is the position of the granule's first row.
fn fold_slots(
    accs: &mut [Vec<Acc>],
    j: usize,
    a: Agg,
    g: &Granule,
    slots: &[Option<usize>],
    at: u64,
) -> Result<()> {
    let pos = |i: usize| at.saturating_add(u64::try_from(i).unwrap_or(u64::MAX));
    let count = |n: &mut u64| -> Result<()> {
        *n = n.checked_add(1).ok_or_else(overflow)?;
        Ok(())
    };
    let Some(c) = a.col() else {
        // COUNT(*): every selected row.
        for s in slots.iter().flatten() {
            if let Some(Acc::Count(n)) = slot(accs, *s, j) {
                count(n)?;
            }
        }
        return Ok(());
    };
    let Some(v) = g.columns.get(c) else {
        // Every value NULL: nothing to fold.
        return Ok(());
    };
    let valid = |i: usize| {
        v.valid
            .as_ref()
            .is_none_or(|x| x.get(i).copied().unwrap_or(false))
    };
    // The selected rows with a value, and their slots.
    let rows = || {
        slots
            .iter()
            .enumerate()
            .filter_map(move |(i, s)| (*s).filter(|_| valid(i)).map(|s| (i, s)))
    };
    match (a, &v.data) {
        (Agg::CountOf(_), _) => {
            for (_, s) in rows() {
                if let Some(Acc::Count(n)) = slot(accs, s, j) {
                    count(n)?;
                }
            }
        }
        (Agg::Sum(_), Data::Int(xs)) => {
            for (i, s) in rows() {
                if let (Some(Acc::Sum { ints, numbers, .. }), Some(x)) =
                    (slot(accs, s, j), xs.get(i))
                {
                    *ints = ints.checked_add(i128::from(*x)).ok_or_else(overflow)?;
                    count(numbers)?;
                }
            }
        }
        (Agg::Sum(_), Data::Real(xs)) => {
            for (i, s) in rows() {
                if let (
                    Some(Acc::Sum {
                        reals,
                        any_real,
                        numbers,
                        ..
                    }),
                    Some(x),
                ) = (slot(accs, s, j), xs.get(i))
                {
                    reals.add(*x);
                    *any_real = true;
                    count(numbers)?;
                }
            }
        }
        (Agg::Sum(_), Data::Text { .. }) => {}
        (Agg::Min(_) | Agg::Max(_), _) => {
            let want_min = matches!(a, Agg::Min(_));
            match extremes(v, rows(), accs.len(), want_min) {
                Some(best) => {
                    for (s, b) in best.into_iter().enumerate() {
                        if let (Some(i), Some(m)) = (b, slot(accs, s, j)) {
                            m.fold(&v.cell(i), pos(i))?;
                        }
                    }
                }
                None => {
                    for (i, s) in rows() {
                        if let Some(m) = slot(accs, s, j) {
                            m.fold(&v.cell(i), pos(i))?;
                        }
                    }
                }
            }
        }
        _ => {
            for (i, s) in rows() {
                if let Some(m) = slot(accs, s, j) {
                    m.fold(&v.cell(i), pos(i))?;
                }
            }
        }
    }
    Ok(())
}

/// Folds a granule's selected rows of one column into an aggregate, over its whole vector.
/// `at` is the position of the granule's first row.
fn fold_vector(
    acc: &mut Acc,
    a: Agg,
    g: &Granule,
    sel: &[bool],
    selected: usize,
    at: u64,
) -> Result<()> {
    let pos = |i: usize| at.saturating_add(u64::try_from(i).unwrap_or(u64::MAX));
    let Some(c) = a.col() else {
        if let Acc::Count(n) = acc {
            *n = n
                .checked_add(u64::try_from(selected).map_err(|_| overflow())?)
                .ok_or_else(overflow)?;
        }
        return Ok(());
    };
    let Some(v) = g.columns.get(c) else {
        // Every value NULL: nothing to fold.
        return Ok(());
    };
    let valid = |i: usize| {
        v.valid
            .as_ref()
            .is_none_or(|x| x.get(i).copied().unwrap_or(false))
    };
    match (acc, &v.data) {
        (Acc::Count(n), _) => {
            let k = match &v.valid {
                None => selected,
                Some(x) => sel.iter().zip(x).filter(|(s, ok)| **s && **ok).count(),
            };
            *n = n
                .checked_add(u64::try_from(k).map_err(|_| overflow())?)
                .ok_or_else(overflow)?;
        }
        (Acc::Sum { ints, numbers, .. }, Data::Int(xs)) => {
            let mut total = 0i128;
            let mut k = 0u64;
            for (i, (x, s)) in xs.iter().zip(sel).enumerate() {
                if *s && valid(i) {
                    total = total.checked_add(i128::from(*x)).ok_or_else(overflow)?;
                    k = k.saturating_add(1);
                }
            }
            *ints = ints.checked_add(total).ok_or_else(overflow)?;
            *numbers = numbers.checked_add(k).ok_or_else(overflow)?;
        }
        (
            Acc::Sum {
                reals,
                any_real,
                numbers,
                ..
            },
            Data::Real(xs),
        ) => {
            let mut k = 0u64;
            for (i, (x, s)) in xs.iter().zip(sel).enumerate() {
                if *s && valid(i) {
                    reals.add(*x);
                    k = k.saturating_add(1);
                }
            }
            *any_real |= k > 0;
            *numbers = numbers.checked_add(k).ok_or_else(overflow)?;
        }
        (Acc::Sum { .. }, Data::Text { .. }) => {}
        (acc @ (Acc::Min(_) | Acc::Max(_)), _) => {
            let want_min = matches!(acc, Acc::Min(_));
            let rows = sel
                .iter()
                .enumerate()
                .filter(|(i, s)| **s && valid(*i))
                .map(|(i, _)| (i, 0));
            match extremes(v, rows, 1, want_min) {
                Some(best) => {
                    for i in best.into_iter().flatten() {
                        acc.fold(&v.cell(i), pos(i))?;
                    }
                }
                None => {
                    for (i, s) in sel.iter().enumerate() {
                        if *s {
                            acc.fold(&v.cell(i), pos(i))?;
                        }
                    }
                }
            }
        }
        (acc, _) => {
            for (i, s) in sel.iter().enumerate() {
                if *s {
                    acc.fold(&v.cell(i), pos(i))?;
                }
            }
        }
    }
    Ok(())
}

/// The decoded granules of the blocks no change touches, for the copy's current and previous
/// generations, keyed by each generation's id. Bound: at most one granule a block of those two,
/// and at most `bound` resident bytes, the share of the database's memory budget its caller
/// charges it to (CLAUDE.md §2); a granule that would pass it isn't kept, and is decoded again by
/// each scan that reads it (counted in [`Sources::uncached`]). A copy is scanned with one
/// [`Decode`] for its life: the cache holds what it decoded.
pub struct Cache {
    bound: usize,
    slots: Mutex<Vec<Slot>>,
}

impl Cache {
    pub fn new(bound: usize) -> Self {
        Cache {
            bound,
            slots: Mutex::new(Vec::new()),
        }
    }

    fn get(&self, made: u64, j: usize) -> Option<Arc<Granule>> {
        self.slots
            .lock()
            .iter()
            .find(|(m, _)| *m == made)
            .and_then(|(_, gs)| gs.get(j).cloned().flatten())
    }

    /// Keeps `g` as generation `made`'s block `j` (of `blocks`), dropping generations no longer
    /// `live` first; false when it would pass the bound, and isn't kept.
    fn put(&self, made: u64, live: &[u64], blocks: usize, j: usize, g: Arc<Granule>) -> bool {
        let mut slots = self.slots.lock();
        slots.retain(|(m, _)| live.contains(m));
        let fits = resident(&slots)
            .checked_add(g.resident())
            .is_some_and(|total| total <= self.bound);
        if !fits {
            return false;
        }
        if !slots.iter().any(|(m, _)| *m == made) {
            slots.push((made, vec![None; blocks]));
        }
        if let Some(slot) = slots
            .iter_mut()
            .find(|(m, _)| *m == made)
            .and_then(|(_, gs)| gs.get_mut(j))
        {
            *slot = Some(g);
        }
        true
    }

    /// The bytes its granules keep resident.
    pub fn resident(&self) -> usize {
        resident(&self.slots.lock())
    }
}

fn resident(slots: &[Slot]) -> usize {
    slots.iter().fold(0usize, |n, (_, gs)| {
        gs.iter()
            .flatten()
            .fold(n, |n, g| n.saturating_add(g.resident()))
    })
}

/// One generation's decoded granules, by block, keyed by its id.
type Slot = (u64, Vec<Option<Arc<Granule>>>);

/// Where a vectorized scan's granules came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sources {
    /// Granules of untouched blocks, decoded before.
    pub cached: u64,
    /// Granules of untouched blocks, decoded now.
    pub decoded: u64,
    /// Granules of blocks with changes at the snapshot, decoded from their rows at it.
    pub changed: u64,
    /// Granules decoded now that the cache's bound left out.
    pub uncached: u64,
    /// Scans answered from the rows: no generation answered them exactly.
    pub fallbacks: u64,
    /// Blocks outside the statement's key range, skipped undecoded.
    pub skipped: u64,
}

impl<B: Block> Columns<B> {
    /// The granules of the rows at snapshot `s`, in key order: one for each of the copy's blocks,
    /// its summaries with it; `rows` the row B-tree at `s`, read for what the copy hasn't applied.
    /// A scan no generation answers exactly reads the rows, cut at the current block size. Blocks
    /// whose keys all fall outside `keys` (by the blocks' key bounds) are skipped undecoded, so
    /// `at` counts the rows of the granules returned.
    pub fn granules(
        &self,
        s: Ts,
        rows: &dyn Rows,
        dec: &dyn Decode,
        cache: &Cache,
        keys: &KeyRange,
    ) -> Result<(Vec<Placed>, Sources)> {
        let st = self.state.read();
        let mut src = Sources::default();
        let mut out = Vec::new();
        let mut at = 0usize;
        let Some((g, skip)) = self.pick(&st, s) else {
            src.fallbacks = 1;
            let all: Rowset = rows.all()?;
            let size = st.sizer.current().unwrap_or(all.len()).max(1);
            for chunk in all.chunks(size) {
                out.push(Placed {
                    at,
                    block: None,
                    granule: Arc::new(Granule::build(chunk, dec)?),
                });
                at = at.saturating_add(chunk.len());
            }
            return Ok((out, src));
        };
        // The queue's keys, by the block each is filed under.
        let mut queued: BTreeMap<usize, BTreeSet<Vec<u8>>> = BTreeMap::new();
        for key in &skip {
            let (j, _) = g.locate(key)?;
            queued.entry(j).or_default().insert(key.clone());
        }
        let live: Vec<u64> = [st.current.as_ref(), st.previous.as_ref()]
            .into_iter()
            .flatten()
            .map(|g| g.id)
            .collect();
        let last_block = g.blocks.len().saturating_sub(1);
        for j in 0..g.blocks.len() {
            // Block j holds the keys after block j - 1's last and up to its own last (the last
            // block, every key after): where `locate` files a change.
            if !keys.is_full() {
                let after = j
                    .checked_sub(1)
                    .and_then(|i| g.blocks.get(i))
                    .map(|b| b.last());
                let upto = g.blocks.get(j).filter(|_| j < last_block).map(|b| b.last());
                if keys.misses(after, upto) {
                    src.skipped = src.skipped.saturating_add(1);
                    continue;
                }
            }
            let touched = g
                .block_deltas(j)
                .any(|(_, versions)| versions.iter().any(|(ts, _)| *ts <= s));
            let held = queued.get(&j);
            let granule = if !touched && held.is_none() {
                match cache.get(g.id, j) {
                    Some(c) => {
                        src.cached = src.cached.saturating_add(1);
                        c
                    }
                    None => {
                        let mut block_rows = Vec::new();
                        g.scan_block(j, s, &skip, &mut block_rows)?;
                        let made = Arc::new(Granule::build(&block_rows, dec)?);
                        if !cache.put(g.id, &live, g.blocks.len(), j, made.clone()) {
                            src.uncached = src.uncached.saturating_add(1);
                        }
                        src.decoded = src.decoded.saturating_add(1);
                        made
                    }
                }
            } else {
                let mut block_rows = Vec::new();
                g.scan_block(j, s, &skip, &mut block_rows)?;
                if let Some(keys) = held {
                    let mut fresh = Vec::new();
                    for key in keys {
                        if let Some(v) = rows.get(key)? {
                            fresh.push((key.clone(), v));
                        }
                    }
                    block_rows.extend(fresh);
                    block_rows.sort_by(|a, b| a.0.cmp(&b.0));
                }
                src.changed = src.changed.saturating_add(1);
                Arc::new(Granule::build(&block_rows, dec)?)
            };
            let rows = granule.rows;
            if rows > 0 {
                out.push(Placed {
                    at,
                    block: Some(j),
                    granule,
                });
            }
            at = at.saturating_add(rows);
        }
        Ok((out, src))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Feed, Packed, Plain};
    use super::*;
    use crate::numeric::nonnan::NonNan;
    use crate::storage::versioned::blink::{Comparator, Descender};
    use crate::storage::versioned::disk::Simulated;
    use crate::storage::versioned::durable::Database;
    use crate::storage::versioned::version::Snapshot;
    use crate::types::{ImmutableRecord, Value};
    use crate::vdbe::morsel::{self, Degree};
    use std::num::NonZeroUsize;
    use std::time::Instant;

    fn cmp() -> Comparator {
        Box::new(|a: &[u8], b: &[u8]| a.cmp(b))
    }

    struct At<'a> {
        db: &'a Database,
        snap: &'a Snapshot,
    }

    impl Rows for At<'_> {
        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
            self.db.get(&mut Descender::new(), self.snap, 0, key)
        }
        fn all(&self) -> Result<Rowset> {
            self.db
                .scan(&mut Descender::new(), self.snap, 0, None, usize::MAX)
        }
    }

    fn rng(x: &mut u64) -> u64 {
        *x ^= *x << 13;
        *x ^= *x >> 7;
        *x ^= *x << 17;
        *x
    }

    fn real(f: f64) -> Value {
        Value::Numeric(Numeric::Float(NonNan::new(f).unwrap()))
    }

    fn int(i: i64) -> Value {
        Value::Numeric(Numeric::Integer(i))
    }

    /// Row `k`'s record: a (integer, rising with the key), b (real or NULL), c (text), d (an
    /// integer, a real, a text or NULL: mixed types), e (the key mod 7, for groups).
    fn record(k: u32, x: &mut u64) -> Vec<u8> {
        let r = rng(x);
        let a = int(i64::from(k) * 3 + i64::try_from(r % 3).unwrap());
        let b = if r.is_multiple_of(5) {
            Value::Null
        } else {
            real(f64::from(u32::try_from(r % 1000).unwrap()) / 7.0)
        };
        let c = Value::from_text(format!("s{:03}", r % 50));
        let d = match r % 4 {
            0 => int(i64::try_from(r % 9).unwrap()),
            1 => real(f64::from(u32::try_from(r % 9).unwrap())),
            2 => Value::from_text(format!("{}", r % 9)),
            _ => Value::Null,
        };
        let e = int(i64::from(k % 7));
        let values = [a, b, c, d, e];
        ImmutableRecord::from_values(values.iter(), values.len())
            .unwrap()
            .get_payload()
            .to_vec()
    }

    fn queries() -> Vec<Query> {
        let budget = usize::MAX;
        let all = vec![
            Agg::Count,
            Agg::CountOf(1),
            Agg::Sum(0),
            Agg::Sum(1),
            Agg::Sum(3),
            Agg::Min(2),
            Agg::Max(3),
            Agg::Min(3),
            Agg::Max(1),
        ];
        let mut qs = vec![
            // count
            Query {
                filters: vec![],
                group: vec![],
                aggs: vec![Agg::Count],
                budget,
                keys: KeyRange::default(),
            },
            // aggregates
            Query {
                filters: vec![],
                group: vec![],
                aggs: all.clone(),
                budget,
                keys: KeyRange::default(),
            },
            // group-by
            Query {
                filters: vec![],
                group: vec![4],
                aggs: all.clone(),
                budget,
                keys: KeyRange::default(),
            },
            Query {
                filters: vec![],
                group: vec![3],
                aggs: vec![Agg::Count, Agg::Sum(0)],
                budget,
                keys: KeyRange::default(),
            },
            Query {
                filters: vec![Filter::Cmp {
                    col: 0,
                    op: Op::Lt,
                    lit: Cell::Int(600),
                }],
                group: vec![2, 4],
                aggs: vec![Agg::Count, Agg::Max(0)],
                budget,
                keys: KeyRange::default(),
            },
        ];
        // filters, each with the aggregates
        let filters = vec![
            Filter::Cmp {
                col: 0,
                op: Op::Eq,
                lit: Cell::Int(300),
            },
            Filter::Cmp {
                col: 0,
                op: Op::Ne,
                lit: Cell::Int(300),
            },
            Filter::Cmp {
                col: 0,
                op: Op::Lt,
                lit: Cell::Int(150),
            },
            Filter::Cmp {
                col: 0,
                op: Op::Le,
                lit: Cell::Int(150),
            },
            Filter::Cmp {
                col: 0,
                op: Op::Gt,
                lit: Cell::Int(1_000),
            },
            Filter::Cmp {
                col: 0,
                op: Op::Ge,
                lit: Cell::Int(1_000),
            },
            Filter::Cmp {
                col: 0,
                op: Op::Lt,
                lit: Cell::Real(450.5),
            },
            Filter::Cmp {
                col: 0,
                op: Op::Eq,
                lit: Cell::Text(b"x".to_vec()),
            },
            Filter::Cmp {
                col: 0,
                op: Op::Lt,
                lit: Cell::Text(b"x".to_vec()),
            },
            Filter::Cmp {
                col: 1,
                op: Op::Gt,
                lit: Cell::Real(70.0),
            },
            Filter::Cmp {
                col: 1,
                op: Op::Le,
                lit: Cell::Int(20),
            },
            Filter::Cmp {
                col: 2,
                op: Op::Ge,
                lit: Cell::Text(b"s025".to_vec()),
            },
            Filter::Cmp {
                col: 2,
                op: Op::Gt,
                lit: Cell::Int(3),
            },
            Filter::Cmp {
                col: 3,
                op: Op::Le,
                lit: Cell::Int(4),
            },
            Filter::Cmp {
                col: 3,
                op: Op::Eq,
                lit: Cell::Real(5.0),
            },
            Filter::Cmp {
                col: 3,
                op: Op::Gt,
                lit: Cell::Text(b"4".to_vec()),
            },
            Filter::Cmp {
                col: 0,
                op: Op::Eq,
                lit: Cell::Null,
            },
            Filter::Cmp {
                col: 9,
                op: Op::Eq,
                lit: Cell::Int(1),
            },
            Filter::IsNull(1),
            Filter::NotNull(3),
            Filter::IsNull(9),
        ];
        for f in filters {
            qs.push(Query {
                filters: vec![f],
                group: vec![],
                aggs: all.clone(),
                budget,
                keys: KeyRange::default(),
            });
        }
        qs.push(Query {
            filters: vec![
                Filter::Cmp {
                    col: 0,
                    op: Op::Ge,
                    lit: Cell::Int(200),
                },
                Filter::Cmp {
                    col: 0,
                    op: Op::Lt,
                    lit: Cell::Int(900),
                },
                Filter::NotNull(1),
            ],
            group: vec![4],
            aggs: all,
            budget,
            keys: KeyRange::default(),
        });
        qs
    }

    /// Every query on `placed` granules, serially and split at random points into partials merged
    /// in order (as morsel workers would run them): each equal to the row-at-a-time answer.
    fn check(placed: &[Placed], rows: &[(Vec<u8>, Vec<Cell>)], x: &mut u64, seen: &mut Counts) {
        for q in queries() {
            let want = evaluate_rows(&q, rows).unwrap();
            let plan = Plan {
                granules: placed.to_vec(),
                query: &q,
            };
            let (got, c) = plan.run_all().unwrap();
            assert_eq!(got, want, "{q:?}");
            seen.merge(&c);
            let mut cuts: Vec<usize> = (0..3)
                .map(|_| usize::try_from(rng(x)).unwrap() % (plan.rows() + 1))
                .collect();
            cuts.push(0);
            cuts.push(plan.rows());
            cuts.sort_unstable();
            let mut total = plan.empty();
            for w in cuts.windows(2) {
                let mut p = plan.empty();
                plan.run(w[0]..w[1], &mut p).unwrap();
                plan.merge(&mut total, p).unwrap();
            }
            assert_eq!(plan.finish(total).unwrap(), want, "split {cuts:?}, {q:?}");
            // The morsel executor, on four workers taking granules one at a time.
            let degree = Degree::Parallel {
                workers: NonZeroUsize::new(4).unwrap(),
                morsel_rows: NonZeroUsize::new(1).unwrap(),
            };
            let (p, stats) =
                morsel::execute_at(&plan, 0, degree, None, &MemoryBudget::new(usize::MAX)).unwrap();
            assert_eq!(plan.finish(p).unwrap(), want, "{stats:?}, {q:?}");
            // And as the executor chooses, on a stated four-core machine.
            let mut costs = MachineCosts::stated(NonZeroUsize::new(4).unwrap(), 1.0, 1.0);
            let (got, counts, stats) = plan
                .run_on_cores(ParallelMode::On, &mut costs, &MemoryBudget::new(usize::MAX))
                .unwrap();
            assert_eq!(got, want, "{stats:?}, {q:?}");
            assert_eq!(counts, c);
        }
    }

    fn decoded(rows: &Rowset) -> Vec<(Vec<u8>, Vec<Cell>)> {
        rows.iter()
            .map(|(k, v)| {
                let mut cells = Vec::new();
                Records.decode(k, v, &mut cells).unwrap();
                (k.clone(), cells)
            })
            .collect()
    }

    /// Key ranges a wide-range statement reads: each kind of bound, and an empty range.
    fn ranges() -> Vec<KeyRange> {
        let k = |n: u32| n.to_be_bytes().to_vec();
        vec![
            KeyRange {
                lo: Bound::Included(k(100)),
                hi: Bound::Excluded(k(300)),
            },
            KeyRange {
                lo: Bound::Excluded(k(350)),
                hi: Bound::Unbounded,
            },
            KeyRange {
                lo: Bound::Unbounded,
                hi: Bound::Included(k(40)),
            },
            KeyRange {
                lo: Bound::Included(k(200)),
                hi: Bound::Included(k(201)),
            },
            KeyRange {
                lo: Bound::Excluded(k(250)),
                hi: Bound::Excluded(k(250)),
            },
        ]
    }

    /// Each range's statements over the granules a range scan returns: equal to the rows', and the
    /// granules hold every key of the range.
    fn check_ranges<B: Block>(
        copy: &Columns<B>,
        snap: Ts,
        rows: &dyn Rows,
        cache: &Cache,
        all: &[(Vec<u8>, Vec<Cell>)],
        seen: &mut Seen,
    ) {
        for range in ranges() {
            let (placed, src) = copy.granules(snap, rows, &Records, cache, &range).unwrap();
            seen.sources.skipped += src.skipped;
            let got_keys: Vec<&[u8]> = placed
                .iter()
                .flat_map(|p| (0..p.granule.rows).map(move |i| p.granule.keys.get(i).unwrap()))
                .filter(|k| range.contains(k))
                .collect();
            let want_keys: Vec<&[u8]> = all
                .iter()
                .map(|(k, _)| k.as_slice())
                .filter(|k| range.contains(k))
                .collect();
            assert_eq!(got_keys, want_keys, "{range:?}");
            for mut q in queries().into_iter().take(6) {
                q.keys = range.clone();
                let want = evaluate_rows(&q, all).unwrap();
                let (got, c) = Plan {
                    granules: placed.clone(),
                    query: &q,
                }
                .run_all()
                .unwrap();
                assert_eq!(got, want, "{range:?} {q:?}");
                seen.counts.merge(&c);
            }
        }
    }

    fn rebuild<B: Block>(db: &Database, copy: &Columns<B>) {
        let t = Instant::now();
        let snap = db.snapshot().unwrap();
        let rows = At { db, snap: &snap }.all().unwrap();
        copy.rebuild(snap.start, &rows, 0).unwrap();
        db.close_snapshot(snap);
        copy.rebuilt(t.elapsed().as_nanos());
    }

    #[derive(Default, Debug)]
    struct Seen {
        counts: Counts,
        sources: Sources,
        held: u64,
    }

    /// Writes between scans (puts and removes, some applied to the copy and some still queued,
    /// rebuilds as the sizer asks), and snapshots held across them: at each, every analytic class
    /// over the copy's granules equals the same statement row by row over the row B-tree.
    fn vectors_equal_rows<B: Block>(seed: u64) -> Seen {
        let db = Database::open(Arc::new(Simulated::new(seed, 1).unwrap()), 4_000, 1, cmp).unwrap();
        let feed = Arc::new(Feed::new(0, usize::MAX, Box::new(|| 0)));
        db.keep_columns(feed.clone()).unwrap();
        let copy = Columns::<B>::new(feed);
        let cache = Cache::new(usize::MAX);
        let mut x = seed | 1;
        let mut d = Descender::new();
        let mut t = db.begin().unwrap();
        for k in 0..400u32 {
            db.put(&mut d, &mut t, 0, &k.to_be_bytes(), &record(k, &mut x))
                .unwrap();
        }
        db.commit(t, &mut d, db.lane(), false).unwrap();
        rebuild(&db, &copy);
        let mut seen = Seen::default();
        let mut held: Vec<Snapshot> = Vec::new();
        for round in 0..40u32 {
            let mut t = db.begin().unwrap();
            for _ in 0..1 + rng(&mut x) % 12 {
                let k = u32::try_from(rng(&mut x) % 480).unwrap();
                if rng(&mut x).is_multiple_of(4) {
                    db.remove(&mut d, &mut t, 0, &k.to_be_bytes()).unwrap();
                } else {
                    let v = record(k, &mut x);
                    db.put(&mut d, &mut t, 0, &k.to_be_bytes(), &v).unwrap();
                }
            }
            db.commit(t, &mut d, db.lane(), false).unwrap();
            if rng(&mut x).is_multiple_of(2) {
                copy.catch_up(0).unwrap();
            }
            if round % 8 == 7 {
                rebuild(&db, &copy);
            }
            let snap = db.snapshot().unwrap();
            let scan = |snap: &Snapshot, seen: &mut Seen, x: &mut u64| {
                let rows = At { db: &db, snap };
                let began = Instant::now();
                let (placed, src) = copy
                    .granules(snap.start, &rows, &Records, &cache, &KeyRange::default())
                    .unwrap();
                let all = rows.all().unwrap();
                copy.measured_scan(began.elapsed().as_nanos(), all.len());
                // Positional summaries: each granule's first row is where it says, keys in order.
                let mut at = 0;
                for p in &placed {
                    assert_eq!(p.at, at);
                    assert_eq!(all[at].0, p.granule.first);
                    at += p.granule.rows;
                    assert_eq!(all[at - 1].0, p.granule.last);
                }
                assert_eq!(at, all.len());
                let cells = decoded(&all);
                check(&placed, &cells, x, &mut seen.counts);
                check_ranges(&copy, snap.start, &rows, &cache, &cells, seen);
                seen.sources.cached += src.cached;
                seen.sources.decoded += src.decoded;
                seen.sources.changed += src.changed;
                seen.sources.fallbacks += src.fallbacks;
            };
            scan(&snap, &mut seen, &mut x);
            if rng(&mut x).is_multiple_of(4) && held.len() < 3 {
                held.push(snap);
            } else {
                db.close_snapshot(snap);
            }
            if !held.is_empty() && rng(&mut x).is_multiple_of(3) {
                let s = held.remove(0);
                scan(&s, &mut seen, &mut x);
                seen.held += 1;
                db.close_snapshot(s);
            }
        }
        for s in held {
            db.close_snapshot(s);
        }
        seen
    }

    /// Do: the differential on prototype A's copy, whole-table and over key ranges. Expect: every
    /// class identical to the rows; the summaries skipped granules, key ranges skipped blocks
    /// undecoded, granules were reused from the cache and decoded for changes (T-CS.4.1, T-CS.4.3,
    /// non-vacuity counters).
    #[test]
    fn cs4_vectors_equal_rows_every_class_plain() {
        let seen = vectors_equal_rows::<Plain>(17);
        assert!(
            seen.counts.skipped > 0
                && seen.sources.skipped > 0
                && seen.sources.cached > 0
                && seen.sources.changed > 0,
            "{seen:?}"
        );
    }

    /// Do: the same on prototype B's compressed blocks. Expect: the same (T-CS.4.1).
    #[test]
    fn cs4_vectors_equal_rows_every_class_packed() {
        let seen = vectors_equal_rows::<Packed>(23);
        assert!(
            seen.counts.skipped > 0
                && seen.sources.skipped > 0
                && seen.sources.cached > 0
                && seen.sources.changed > 0,
            "{seen:?}"
        );
    }

    /// Do: cut 100 rows (a = 0..99) into granules of 10 and filter a > 85. Expect: the summaries
    /// skip the eight granules below 80 unread, and the answer equals the rows' (T-CS.4.2).
    #[test]
    fn cs4_min_max_summaries_skip_granules() {
        let rows: Rowset = (0..100u32)
            .map(|k| {
                let values = [int(i64::from(k)), real(f64::from(k) / 2.0)];
                (
                    k.to_be_bytes().to_vec(),
                    ImmutableRecord::from_values(values.iter(), 2)
                        .unwrap()
                        .get_payload()
                        .to_vec(),
                )
            })
            .collect();
        let placed: Vec<Placed> = rows
            .chunks(10)
            .enumerate()
            .map(|(i, c)| Placed {
                at: i * 10,
                block: Some(i),
                granule: Arc::new(Granule::build(c, &Records).unwrap()),
            })
            .collect();
        let q = Query {
            filters: vec![Filter::Cmp {
                col: 0,
                op: Op::Gt,
                lit: Cell::Int(85),
            }],
            group: vec![],
            aggs: vec![Agg::Count, Agg::Sum(0), Agg::Sum(1), Agg::Min(0)],
            budget: usize::MAX,
            keys: KeyRange::default(),
        };
        let (got, c) = Plan {
            granules: placed,
            query: &q,
        }
        .run_all()
        .unwrap();
        assert_eq!(got, evaluate_rows(&q, &decoded(&rows)).unwrap());
        assert_eq!(
            got,
            vec![vec![
                Cell::Int(14),
                Cell::Int((86..100).sum()),
                Cell::Real(f64::from((86..100).sum::<u32>()) / 2.0),
                Cell::Int(86)
            ]]
        );
        assert_eq!((c.skipped, c.granules, c.rows, c.selected), (8, 2, 20, 14));
    }

    /// Do: scan a copy of several blocks through a cache whose bound holds none of them, twice.
    /// Expect: nothing kept, every untouched granule decoded again and counted, answers still equal
    /// the rows' (CLAUDE.md §2).
    #[test]
    fn cs4_granule_cache_keeps_to_its_bound() {
        let db = Database::open(Arc::new(Simulated::new(29, 1).unwrap()), 2_000, 1, cmp).unwrap();
        let feed = Arc::new(Feed::new(0, usize::MAX, Box::new(|| 0)));
        db.keep_columns(feed.clone()).unwrap();
        let copy = Columns::<Plain>::new(feed);
        let mut x = 9u64;
        let mut d = Descender::new();
        let mut t = db.begin().unwrap();
        for k in 0..200u32 {
            db.put(&mut d, &mut t, 0, &k.to_be_bytes(), &record(k, &mut x))
                .unwrap();
        }
        db.commit(t, &mut d, db.lane(), false).unwrap();
        rebuild(&db, &copy);
        let cache = Cache::new(0);
        for _ in 0..2 {
            let snap = db.snapshot().unwrap();
            let rows = At {
                db: &db,
                snap: &snap,
            };
            let (placed, src) = copy
                .granules(snap.start, &rows, &Records, &cache, &KeyRange::default())
                .unwrap();
            assert_eq!((src.cached, src.uncached), (0, src.decoded));
            assert!(src.decoded > 0);
            let qs = queries();
            let q = &qs[1];
            let (got, _) = Plan {
                granules: placed,
                query: q,
            }
            .run_all()
            .unwrap();
            assert_eq!(
                got,
                evaluate_rows(q, &decoded(&rows.all().unwrap())).unwrap()
            );
            db.close_snapshot(snap);
        }
        assert_eq!(cache.resident(), 0);
    }

    /// Do: group 50 distinct keys under a budget that holds fewer. Expect: a typed refusal, no
    /// group past the budget (CLAUDE.md §2).
    #[test]
    fn cs4_groups_past_budget_are_refused() {
        let rows: Vec<(Vec<u8>, Vec<Cell>)> = (0..50u8)
            .map(|i| (vec![i], vec![Cell::Int(i64::from(i))]))
            .collect();
        let q = Query {
            filters: vec![],
            group: vec![0],
            aggs: vec![Agg::Count],
            budget: 200,
            keys: KeyRange::default(),
        };
        assert!(matches!(
            evaluate_rows(&q, &rows),
            Err(LimboError::OutOfMemory)
        ));
    }

    /// Do: compare integers and reals at the edges of a double's precision. Expect: exact answers,
    /// where a cast would round (SQLite's integer-real comparison).
    #[test]
    fn cs4_integers_and_reals_compare_exactly() {
        let two53 = 9_007_199_254_740_992.0f64;
        assert_eq!(int_real(9_007_199_254_740_993, two53), Ordering::Greater);
        assert_eq!(int_real(9_007_199_254_740_992, two53), Ordering::Equal);
        assert_eq!(
            int_real(i64::MAX, 9_223_372_036_854_775_808.0),
            Ordering::Less
        );
        assert_eq!(
            int_real(i64::MIN, -9_223_372_036_854_775_808.0),
            Ordering::Equal
        );
        assert_eq!(int_real(3, 3.5), Ordering::Less);
        assert_eq!(int_real(-3, -3.5), Ordering::Greater);
        assert_eq!(int_real(0, -0.0), Ordering::Equal);
        let mut s = ExactSum::default();
        for v in [1e100, 1.0, -1e100, 0.25] {
            s.add(v);
        }
        assert_eq!(s.value(), 1.25);
    }

    /// Do: group rows whose keys mix integers with equal reals, nulls and text, under every
    /// aggregate, over granules of each size from one row to all of them, run whole and merged one
    /// granule at a time from the last. Expect: each answer equals the statement row by row,
    /// including which of a group's equal keys it shows (the earliest row's) (T-CS.4.5).
    #[test]
    fn cs4_grouped_aggregates_fold_whole_vectors() {
        struct Table(Vec<Vec<Cell>>);
        impl Decode for Table {
            fn decode(&self, _key: &[u8], row: &[u8], out: &mut Vec<Cell>) -> Result<()> {
                let i = row
                    .get(..2)
                    .and_then(|b| <[u8; 2]>::try_from(b).ok())
                    .map(u16::from_be_bytes)
                    .ok_or_else(|| LimboError::InternalError("short row".into()))?;
                out.extend(
                    self.0
                        .get(usize::from(i))
                        .ok_or_else(|| LimboError::InternalError("no such row".into()))?
                        .iter()
                        .cloned(),
                );
                Ok(())
            }
        }
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut cells = Vec::new();
        for i in 0..240u16 {
            let r = next();
            let k = i64::try_from(r % 4).unwrap();
            let key = match r % 5 {
                0 => Cell::Int(k),
                1 => Cell::Real(k as f64),
                2 => Cell::Null,
                3 => Cell::Text(vec![b'a', u8::try_from(r % 3).unwrap()]),
                _ => Cell::Int(-k),
            };
            let v = match (r >> 8) % 4 {
                0 => Cell::Int(i64::from(i) - 100),
                1 => Cell::Real(f64::from(i) * 0.5),
                2 => Cell::Null,
                _ => Cell::Int(i64::try_from((r >> 16) % 7).unwrap()),
            };
            let int = Cell::Int(i64::try_from((r >> 24) % 9).unwrap() - 4);
            let text = Cell::Text(vec![b'x', u8::try_from((r >> 32) % 5).unwrap()]);
            cells.push(vec![key, v, int, text]);
        }
        let rows: Vec<(Vec<u8>, Vec<u8>)> = (0..240u16)
            .map(|i| {
                (
                    u32::from(i).to_be_bytes().to_vec(),
                    i.to_be_bytes().to_vec(),
                )
            })
            .collect();
        let decoded: Vec<(Vec<u8>, Vec<Cell>)> = rows
            .iter()
            .zip(&cells)
            .map(|((k, _), c)| (k.clone(), c.clone()))
            .collect();
        let table = Table(cells);
        let aggs = vec![
            Agg::Count,
            Agg::CountOf(1),
            Agg::Sum(1),
            Agg::Min(1),
            Agg::Max(1),
            Agg::Sum(2),
            Agg::Min(2),
            Agg::Max(2),
            Agg::CountOf(3),
            Agg::Min(3),
            Agg::Max(3),
            Agg::Sum(3),
        ];
        for group in [vec![0], vec![0, 3], vec![2]] {
            for filters in [vec![], vec![Filter::NotNull(1)]] {
                let q = Query {
                    filters,
                    group: group.clone(),
                    aggs: aggs.clone(),
                    budget: usize::MAX,
                    keys: KeyRange::default(),
                };
                let want = evaluate_rows(&q, &decoded).unwrap();
                for size in [1, 7, 64, 240] {
                    let mut granules = Vec::new();
                    for (n, chunk) in rows.chunks(size).enumerate() {
                        granules.push(Placed {
                            at: n * size,
                            block: None,
                            granule: Arc::new(Granule::build(chunk, &table).unwrap()),
                        });
                    }
                    let plan = Plan {
                        granules,
                        query: &q,
                    };
                    assert_eq!(plan.run_all().unwrap().0, want, "{group:?} at {size}");
                    let mut later = plan.empty();
                    for i in (0..plan.rows()).rev() {
                        let mut p = plan.empty();
                        plan.run(i..i + 1, &mut p).unwrap();
                        plan.merge(&mut p, later).unwrap();
                        later = p;
                    }
                    assert_eq!(
                        plan.finish(later).unwrap(),
                        want,
                        "{group:?} at {size}, merged"
                    );
                }
            }
        }
    }

    /// Do: decode every truncation of a record. Expect: a value or a typed error, never a panic
    /// (hostile input).
    #[test]
    fn cs4_damaged_records_do_not_panic() {
        let mut x = 3u64;
        let r = record(7, &mut x);
        for cut in 0..r.len() {
            let _ = Granule::build(&[(vec![1], r[..cut].to_vec())], &Records);
        }
    }
}
