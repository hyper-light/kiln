//! Column compression for the copy of `docs/design/columnar.md` decision 1 (G11 step 3, CS.3):
//! BtrBlocks' cascade [c86][c87] against FastLanes' layouts [c88][c89][c90], both in safe Rust,
//! with no recursion (CLAUDE.md §11, [c10]) and no picked sizes.
//!
//! Both codecs take a column of integers, doubles or strings, with or without nulls, and choose
//! each column's scheme from a sample of it, as BtrBlocks does: every scheme the codec has is
//! tried on the sample, the one that leaves the fewest bits a value wins, and the column (or a
//! part a scheme splits off, such as a dictionary's codes) is encoded with it. A part is chosen for
//! again the same way, so the cascade's depth is whatever the samples choose; a scheme already used
//! on the way to a part is not offered to it again, so it ends (CLAUDE.md §2: its depth is bounded
//! by the codec's scheme count, a format value). The sample grows until its choice stops changing
//! ([`cascade`]), so neither its size nor the depth is a constant.
//!
//! The codecs differ in their schemes and their bit-packing ([`pack`]):
//! - [`Codec::BtrBlocks`]: one value, run lengths, dictionary, decimal for doubles, and frame of
//!   reference with horizontal bit-packing.
//! - [`Codec::FastLanes`]: one value, dictionary, decimal for doubles, DELTA, and frame of
//!   reference packed in FastLanes' interleaved 1,024-value layout.
//!
//! Nulls are a bitmap beside the values; a null's slot carries the value before it, so it costs
//! nothing in runs, dictionaries or packing.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

pub mod cascade;
pub mod pack;

use std::fmt;

/// What can't be encoded or decoded, and why (CLAUDE.md §3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodecError {
    /// A size or offset outgrew its type.
    Overflow,
    /// Encoded data that doesn't hold together: what is wrong with it.
    Corrupt(&'static str),
    /// A column whose parts disagree: what is wrong with it.
    Invalid(&'static str),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::Overflow => write!(f, "a size or offset outgrew its type"),
            CodecError::Corrupt(why) => write!(f, "corrupt encoding: {why}"),
            CodecError::Invalid(why) => write!(f, "invalid column: {why}"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Strings as one byte buffer and the offsets that cut it: string `i` is
/// `bytes[offsets[i]..offsets[i + 1]]`; `offsets` starts at 0 and has one more entry than strings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Strings {
    pub bytes: Vec<u8>,
    pub offsets: Vec<usize>,
}

impl Strings {
    pub fn new() -> Self {
        Strings {
            bytes: Vec::new(),
            offsets: vec![0],
        }
    }

    pub fn push(&mut self, s: &[u8]) {
        self.bytes.extend_from_slice(s);
        self.offsets.push(self.bytes.len());
    }

    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// String `i`, or `None` past the end or where the offsets don't cut the bytes.
    pub fn get(&self, i: usize) -> Option<&[u8]> {
        let start = *self.offsets.get(i)?;
        let end = *self.offsets.get(i.checked_add(1)?)?;
        self.bytes.get(start..end)
    }

    /// Every string, in order; `None` for an entry whose offsets don't cut the bytes.
    pub fn iter(&self) -> impl Iterator<Item = Option<&[u8]>> + '_ {
        self.offsets.windows(2).map(|w| match w {
            [s, e] => self.bytes.get(*s..*e),
            _ => None,
        })
    }

    pub(crate) fn check(&self) -> Result<(), CodecError> {
        if self.offsets.first() != Some(&0)
            || self.offsets.last() != Some(&self.bytes.len())
            || self
                .offsets
                .windows(2)
                .any(|w| matches!(w, [s, e] if s > e))
        {
            return Err(CodecError::Invalid(
                "string offsets do not cut their bytes in order",
            ));
        }
        Ok(())
    }
}

/// A column's values.
#[derive(Clone, Debug, PartialEq)]
pub enum Values {
    Int(Vec<i64>),
    Double(Vec<f64>),
    Str(Strings),
}

impl Values {
    pub fn len(&self) -> usize {
        match self {
            Values::Int(v) => v.len(),
            Values::Double(v) => v.len(),
            Values::Str(s) => s.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A column: its values and, if any is null, which are valid. A null's slot in `values` holds
/// whatever its writer put there; [`Column::equal_rows`] ignores it.
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub values: Values,
    pub valid: Option<Vec<bool>>,
}

impl Column {
    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn is_valid(&self, i: usize) -> bool {
        self.valid
            .as_ref()
            .is_none_or(|v| v.get(i).copied().unwrap_or(false))
    }

    /// Whether the two columns hold the same rows: the same nulls, and bit-for-bit the same value
    /// at every row that isn't null.
    pub fn equal_rows(&self, other: &Column) -> bool {
        if self.len() != other.len() {
            return false;
        }
        (0..self.len()).all(|i| {
            let (a, b) = (self.is_valid(i), other.is_valid(i));
            if a != b {
                return false;
            }
            if !a {
                return true;
            }
            match (&self.values, &other.values) {
                (Values::Int(x), Values::Int(y)) => x.get(i) == y.get(i),
                (Values::Double(x), Values::Double(y)) => {
                    x.get(i).map(|v| v.to_bits()) == y.get(i).map(|v| v.to_bits())
                }
                (Values::Str(x), Values::Str(y)) => x.get(i) == y.get(i),
                _ => false,
            }
        })
    }
}

/// The two codecs compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    BtrBlocks,
    FastLanes,
}

/// An encoded column: the null bitmap, if any, and the cascade's nodes.
#[derive(Clone, Debug, PartialEq)]
pub struct Encoded {
    pub codec: Codec,
    pub len: usize,
    /// One bit a row, set where the row is valid; absent when none is null.
    pub valid: Option<Vec<u64>>,
    pub tree: cascade::Tree,
}

impl Encoded {
    /// The bytes the encoding takes: every buffer, plus each node's scheme and lengths.
    pub fn bytes(&self) -> usize {
        let valid = self.valid.as_ref().map_or(0, |w| w.len().saturating_mul(8));
        self.tree.bytes().saturating_add(valid)
    }

    /// Bits a value: [`Encoded::bytes`] over the rows.
    pub fn bits_per_value(&self) -> f64 {
        if self.len == 0 {
            return 0.0;
        }
        (self.bytes() as f64) * 8.0 / (self.len as f64)
    }

    /// The column back, bit for bit at every valid row.
    pub fn decode(&self) -> Result<Column, CodecError> {
        let values = self.tree.decode()?;
        if values.len() != self.len {
            return Err(CodecError::Corrupt(
                "decoded length differs from the encoded one",
            ));
        }
        let valid = match &self.valid {
            None => None,
            Some(words) => {
                let mut v = Vec::with_capacity(self.len);
                for w in words {
                    v.extend((0..64u32).map(|b| w.wrapping_shr(b) & 1 == 1));
                }
                if v.len() < self.len {
                    return Err(CodecError::Corrupt("null bitmap shorter than the column"));
                }
                v.truncate(self.len);
                Some(v)
            }
        };
        Ok(Column { values, valid })
    }
}

/// Encodes `column` with `codec`, each scheme chosen from samples of the data.
pub fn encode(codec: Codec, column: &Column) -> Result<Encoded, CodecError> {
    let len = column.len();
    let (values, valid) = match &column.valid {
        None => (column.values.clone(), None),
        Some(v) if v.len() != len => {
            return Err(CodecError::Invalid(
                "validity length differs from the values'",
            ));
        }
        Some(v) if v.iter().all(|&b| b) => (column.values.clone(), None),
        Some(v) => (fill_nulls(&column.values, v)?, Some(bitmap(v))),
    };
    if let Values::Str(s) = &values {
        s.check()?;
    }
    let tree = cascade::encode(codec, values)?;
    Ok(Encoded {
        codec,
        len,
        valid,
        tree,
    })
}

fn bitmap(valid: &[bool]) -> Vec<u64> {
    valid
        .chunks(64)
        .map(|c| {
            (0u32..).zip(c).fold(
                0u64,
                |w, (b, &on)| if on { w | 1u64.wrapping_shl(b) } else { w },
            )
        })
        .collect()
}

/// The values with each null's slot set to the value before it (or the type's zero before the
/// first valid one), so nulls lengthen runs and add nothing to dictionaries or bit widths.
fn fill_nulls(values: &Values, valid: &[bool]) -> Result<Values, CodecError> {
    Ok(match values {
        Values::Int(v) => Values::Int(fill(v, valid, 0)),
        Values::Double(v) => Values::Double(fill(v, valid, 0.0)),
        Values::Str(s) => {
            s.check()?;
            let mut out = Strings::new();
            let mut last: &[u8] = &[];
            for (entry, &ok) in s.iter().zip(valid) {
                let entry =
                    entry.ok_or(CodecError::Invalid("string offsets do not cut their bytes"))?;
                if ok {
                    last = entry;
                }
                out.push(last);
            }
            Values::Str(out)
        }
    })
}

fn fill<T: Copy>(v: &[T], valid: &[bool], zero: T) -> Vec<T> {
    let mut last = zero;
    v.iter()
        .zip(valid)
        .map(|(&x, &ok)| {
            if ok {
                last = x;
            }
            last
        })
        .collect()
}
