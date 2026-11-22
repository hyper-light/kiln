//! The cascade: each column, and each part a scheme splits off, encoded with the scheme a sample
//! of it chooses (BtrBlocks' method [c87]), as a loop over a queue of parts (CLAUDE.md §11).
//!
//! The nodes are kept flat: a node's parts always come after it, so decoding walks the nodes from
//! last to first and every part is decoded before the node that needs it.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;

use crate::pack::{self, GROUP, Layout, Packed, width_of};
use crate::{Codec, CodecError, Strings, Values};

/// A scheme a node can take.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scheme {
    /// The values as they are.
    Raw,
    /// One value, repeated.
    One,
    /// Frame of reference and bit-packing, in the codec's layout.
    Packed,
    /// FastLanes' DELTA: lane-wise differences, packed in its layout.
    Delta,
    /// Run lengths: each run's value and its length, two parts.
    Rle,
    /// A dictionary of the distinct values and each row's code into it, two parts.
    Dict,
    /// Doubles as decimal digits and a power of ten, two parts, with the values that aren't
    /// patched in whole (BtrBlocks' pseudodecimal; FastLanes' group pairs it with ALP).
    Decimal,
}

impl Scheme {
    fn bit(self) -> u8 {
        match self {
            Scheme::Raw => 1,
            Scheme::One => 2,
            Scheme::Packed => 4,
            Scheme::Delta => 8,
            Scheme::Rle => 16,
            Scheme::Dict => 32,
            Scheme::Decimal => 64,
        }
    }
}

/// The schemes each codec offers, by the type of the values.
fn candidates(codec: Codec, kind: Kind) -> &'static [Scheme] {
    use Scheme::*;
    match (codec, kind) {
        (Codec::BtrBlocks, Kind::Int) => &[Raw, One, Packed, Rle, Dict],
        (Codec::BtrBlocks, Kind::Double) => &[Raw, One, Rle, Dict, Decimal],
        (Codec::FastLanes, Kind::Int) => &[Raw, One, Packed, Delta, Dict],
        (Codec::FastLanes, Kind::Double) => &[Raw, One, Dict, Decimal],
        (_, Kind::Str) => &[Raw, One, Dict],
    }
}

fn layout(codec: Codec) -> Layout {
    match codec {
        Codec::BtrBlocks => Layout::Horizontal,
        Codec::FastLanes => Layout::Interleaved,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Int,
    Double,
    Str,
}

/// A node of the cascade. `usize` fields name the node holding that part.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    IntRaw(Vec<u64>),
    IntOne {
        value: u64,
        len: usize,
    },
    IntPacked(Packed),
    IntDelta {
        bases: Vec<u64>,
        deltas: Packed,
    },
    IntRle {
        len: usize,
        values: usize,
        lengths: usize,
    },
    IntDict {
        dict: usize,
        codes: usize,
    },
    DblRaw(Vec<f64>),
    DblOne {
        value: f64,
        len: usize,
    },
    DblRle {
        len: usize,
        values: usize,
        lengths: usize,
    },
    DblDict {
        dict: usize,
        codes: usize,
    },
    DblDecimal {
        digits: usize,
        exponents: usize,
        patches: Vec<(usize, f64)>,
    },
    StrRaw {
        bytes: Vec<u8>,
        lengths: usize,
    },
    StrOne {
        value: Vec<u8>,
        len: usize,
    },
    StrDict {
        dict: usize,
        codes: usize,
    },
}

/// An encoded column's nodes; the first is the column.
#[derive(Clone, Debug, PartialEq)]
pub struct Tree {
    pub nodes: Vec<Node>,
}

/// Values while they are encoded or decoded; integers as their two's complement bits.
enum Data {
    Int(Vec<u64>),
    Dbl(Vec<f64>),
    Str(Strings),
}

impl Data {
    fn len(&self) -> usize {
        match self {
            Data::Int(v) => v.len(),
            Data::Dbl(v) => v.len(),
            Data::Str(s) => s.len(),
        }
    }
}

/// Bytes a length or offset field takes in the counted size: a format value.
const LEN_BYTES: usize = 4;
/// Bytes of a node's scheme tag: a format value.
const TAG_BYTES: usize = 1;

impl Tree {
    /// The bytes the nodes take: buffers, plus each node's tag and its lengths and bases.
    pub fn bytes(&self) -> usize {
        self.nodes.iter().fold(0usize, |sum, node| {
            let packed = |p: &Packed| {
                p.words
                    .len()
                    .saturating_mul(8)
                    .saturating_add(8 + 1 + LEN_BYTES)
            };
            let own = match node {
                Node::IntRaw(v) => v.len().saturating_mul(8).saturating_add(LEN_BYTES),
                Node::IntOne { .. } | Node::DblOne { .. } => 8 + LEN_BYTES,
                Node::IntPacked(p) => packed(p),
                Node::IntDelta { bases, deltas } => {
                    packed(deltas).saturating_add(bases.len().saturating_mul(8))
                }
                Node::IntRle { .. } | Node::DblRle { .. } => LEN_BYTES,
                Node::IntDict { .. } | Node::DblDict { .. } | Node::StrDict { .. } => 0,
                Node::DblRaw(v) => v.len().saturating_mul(8).saturating_add(LEN_BYTES),
                Node::DblDecimal { patches, .. } => patches
                    .len()
                    .saturating_mul(LEN_BYTES + 8)
                    .saturating_add(LEN_BYTES),
                Node::StrRaw { bytes, .. } => bytes.len(),
                Node::StrOne { value, .. } => value.len().saturating_add(LEN_BYTES),
            };
            sum.saturating_add(own).saturating_add(TAG_BYTES)
        })
    }

    /// Decodes every node, last to first, and returns the column's values.
    pub fn decode(&self) -> Result<Values, CodecError> {
        let mut done: Vec<Option<Data>> = Vec::with_capacity(self.nodes.len());
        done.resize_with(self.nodes.len(), || None);
        for (at, node) in self.nodes.iter().enumerate().rev() {
            let data = decode_node(at, node, &mut done)?;
            *done.get_mut(at).ok_or(CodecError::Overflow)? = Some(data);
        }
        match done.first_mut().and_then(Option::take) {
            Some(Data::Int(v)) => Ok(Values::Int(v.into_iter().map(u64::cast_signed).collect())),
            Some(Data::Dbl(v)) => Ok(Values::Double(v)),
            Some(Data::Str(s)) => Ok(Values::Str(s)),
            None => Err(CodecError::Corrupt("no column node")),
        }
    }
}

/// Takes node `part`'s decoded values, once; a part must come after the node `at` that uses it.
fn take(done: &mut [Option<Data>], at: usize, part: usize) -> Result<Data, CodecError> {
    if part <= at {
        return Err(CodecError::Corrupt(
            "a part comes before the node that uses it",
        ));
    }
    done.get_mut(part)
        .and_then(Option::take)
        .ok_or(CodecError::Corrupt("a part is missing or used twice"))
}

fn take_int(done: &mut [Option<Data>], at: usize, part: usize) -> Result<Vec<u64>, CodecError> {
    match take(done, at, part)? {
        Data::Int(v) => Ok(v),
        _ => Err(CodecError::Corrupt("an integer part holds another type")),
    }
}

fn take_dbl(done: &mut [Option<Data>], at: usize, part: usize) -> Result<Vec<f64>, CodecError> {
    match take(done, at, part)? {
        Data::Dbl(v) => Ok(v),
        _ => Err(CodecError::Corrupt("a double part holds another type")),
    }
}

fn take_str(done: &mut [Option<Data>], at: usize, part: usize) -> Result<Strings, CodecError> {
    match take(done, at, part)? {
        Data::Str(v) => Ok(v),
        _ => Err(CodecError::Corrupt("a string part holds another type")),
    }
}

/// Run `lengths` expanded: each value repeated its run's length, `len` in all.
fn expand<T: Copy>(values: &[T], lengths: &[u64], len: usize) -> Result<Vec<T>, CodecError> {
    if values.len() != lengths.len() {
        return Err(CodecError::Corrupt(
            "run values and lengths differ in number",
        ));
    }
    let total = lengths
        .iter()
        .try_fold(0u64, |s, &l| s.checked_add(l))
        .ok_or(CodecError::Overflow)?;
    if usize::try_from(total).ok() != Some(len) {
        return Err(CodecError::Corrupt(
            "run lengths do not add up to the column",
        ));
    }
    let mut out = Vec::with_capacity(len);
    for (&v, &l) in values.iter().zip(lengths) {
        out.extend(std::iter::repeat_n(
            v,
            usize::try_from(l).map_err(|_| CodecError::Overflow)?,
        ));
    }
    Ok(out)
}

/// Each code looked up in `dict`; a code past its end refuses the whole lookup.
fn lookup<T: Copy + Default>(dict: &[T], codes: &[u64]) -> Result<Vec<T>, CodecError> {
    let mut bad = false;
    let out = codes
        .iter()
        .map(
            |&c| match usize::try_from(c).ok().and_then(|c| dict.get(c)) {
                Some(&v) => v,
                None => {
                    bad = true;
                    T::default()
                }
            },
        )
        .collect();
    if bad {
        return Err(CodecError::Corrupt(
            "a dictionary code past the dictionary's end",
        ));
    }
    Ok(out)
}

/// The exact powers of ten in a double: 10^22 is the largest (5^22 < 2^53), a format value.
const POW10: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

fn decode_node(at: usize, node: &Node, done: &mut [Option<Data>]) -> Result<Data, CodecError> {
    Ok(match node {
        Node::IntRaw(v) => Data::Int(v.clone()),
        Node::IntOne { value, len } => Data::Int(vec![*value; *len]),
        Node::IntPacked(p) => {
            let mut out = Vec::new();
            pack::unpack(p, &mut out)?;
            Data::Int(out)
        }
        Node::IntDelta { bases, deltas } => {
            let mut out = Vec::new();
            pack::unpack(deltas, &mut out)?;
            pack::delta_decode(&mut out, bases)?;
            Data::Int(out)
        }
        Node::IntRle {
            len,
            values,
            lengths,
        } => {
            let lengths = take_int(done, at, *lengths)?;
            let values = take_int(done, at, *values)?;
            Data::Int(expand(&values, &lengths, *len)?)
        }
        Node::IntDict { dict, codes } => {
            let codes = take_int(done, at, *codes)?;
            let dict = take_int(done, at, *dict)?;
            Data::Int(lookup(&dict, &codes)?)
        }
        Node::DblRaw(v) => Data::Dbl(v.clone()),
        Node::DblOne { value, len } => Data::Dbl(vec![*value; *len]),
        Node::DblRle {
            len,
            values,
            lengths,
        } => {
            let lengths = take_int(done, at, *lengths)?;
            let values = take_dbl(done, at, *values)?;
            Data::Dbl(expand(&values, &lengths, *len)?)
        }
        Node::DblDict { dict, codes } => {
            let codes = take_int(done, at, *codes)?;
            let dict = take_dbl(done, at, *dict)?;
            Data::Dbl(lookup(&dict, &codes)?)
        }
        Node::DblDecimal {
            digits,
            exponents,
            patches,
        } => {
            let exponents = take_int(done, at, *exponents)?;
            let digits = take_int(done, at, *digits)?;
            if digits.len() != exponents.len() {
                return Err(CodecError::Corrupt(
                    "decimal digits and exponents differ in number",
                ));
            }
            let mut bad = false;
            let mut out: Vec<f64> = digits
                .iter()
                .zip(&exponents)
                .map(
                    |(&d, &e)| match usize::try_from(e).ok().and_then(|e| POW10.get(e)) {
                        Some(p) => (d.cast_signed() as f64) / p,
                        None => {
                            bad = true;
                            0.0
                        }
                    },
                )
                .collect();
            if bad {
                return Err(CodecError::Corrupt("a decimal exponent past 22"));
            }
            for &(i, v) in patches {
                *out.get_mut(i)
                    .ok_or(CodecError::Corrupt("a decimal patch past the column's end"))? = v;
            }
            Data::Dbl(out)
        }
        Node::StrRaw { bytes, lengths } => {
            let lengths = take_int(done, at, *lengths)?;
            let mut offsets = Vec::with_capacity(lengths.len().saturating_add(1));
            offsets.push(0usize);
            let mut end = 0usize;
            for l in lengths {
                end = usize::try_from(l)
                    .ok()
                    .and_then(|l| end.checked_add(l))
                    .ok_or(CodecError::Overflow)?;
                offsets.push(end);
            }
            if end != bytes.len() {
                return Err(CodecError::Corrupt(
                    "string lengths do not add up to their bytes",
                ));
            }
            Data::Str(Strings {
                bytes: bytes.clone(),
                offsets,
            })
        }
        Node::StrOne { value, len } => {
            let mut s = Strings {
                bytes: Vec::with_capacity(value.len().saturating_mul(*len)),
                offsets: Vec::with_capacity(len.saturating_add(1)),
            };
            s.offsets.push(0);
            for _ in 0..*len {
                s.push(value);
            }
            Data::Str(s)
        }
        Node::StrDict { dict, codes } => {
            let codes = take_int(done, at, *codes)?;
            let dict = take_str(done, at, *dict)?;
            let mut s = Strings {
                bytes: Vec::new(),
                offsets: Vec::with_capacity(codes.len().saturating_add(1)),
            };
            s.offsets.push(0);
            for c in codes {
                let entry = usize::try_from(c).ok().and_then(|c| dict.get(c)).ok_or(
                    CodecError::Corrupt("a dictionary code past the dictionary's end"),
                )?;
                s.push(entry);
            }
            Data::Str(s)
        }
    })
}

// ---------------------------------------------------------------------------------------------
// Encoding

/// A part still to encode: the node it fills, its values, and the schemes used on the way to it.
struct Task {
    at: usize,
    data: Data,
    used: u8,
}

/// Encodes `values` with `codec`.
pub fn encode(codec: Codec, values: Values) -> Result<Tree, CodecError> {
    let root = match values {
        Values::Int(v) => Data::Int(v.into_iter().map(i64::cast_unsigned).collect()),
        Values::Double(v) => Data::Dbl(v),
        Values::Str(s) => Data::Str(s),
    };
    let mut nodes = vec![Node::IntOne { value: 0, len: 0 }];
    let mut queue = VecDeque::from([Task {
        at: 0,
        data: root,
        used: 0,
    }]);
    // Bounded: a part's schemes exclude those used on its way, so no path is longer than the
    // codec's schemes, and each node has at most two parts (CLAUDE.md §2).
    while let Some(task) = queue.pop_front() {
        let node = encode_part(codec, task.data, task.used, &mut nodes, &mut queue)?;
        *nodes.get_mut(task.at).ok_or(CodecError::Overflow)? = node;
    }
    Ok(Tree { nodes })
}

/// Queues `data` as a part of a node encoded with `scheme`, and returns the node it will fill.
fn part(nodes: &mut Vec<Node>, queue: &mut VecDeque<Task>, data: Data, used: u8) -> usize {
    let at = nodes.len();
    nodes.push(Node::IntOne { value: 0, len: 0 });
    queue.push_back(Task { at, data, used });
    at
}

fn encode_part(
    codec: Codec,
    data: Data,
    used: u8,
    nodes: &mut Vec<Node>,
    queue: &mut VecDeque<Task>,
) -> Result<Node, CodecError> {
    let kind = match &data {
        Data::Int(_) => Kind::Int,
        Data::Dbl(_) => Kind::Double,
        Data::Str(_) => Kind::Str,
    };
    let offered: Vec<Scheme> = candidates(codec, kind)
        .iter()
        .copied()
        .filter(|s| s.bit() & used == 0)
        .collect();
    let ranked = choose(data.len(), |ranges| rank(&data, ranges, &offered))?;
    // The best scheme that applies to the whole part: a sample can see one value where the part
    // has more.
    let scheme = ranked
        .into_iter()
        .find(|&s| s != Scheme::One || is_one(&data))
        .unwrap_or(Scheme::Raw);
    let used = used | scheme.bit();
    let lay = layout(codec);
    Ok(match (data, scheme) {
        (Data::Int(v), Scheme::One) => Node::IntOne {
            value: v.first().copied().unwrap_or(0),
            len: v.len(),
        },
        (Data::Int(v), Scheme::Packed) => Node::IntPacked(for_pack(lay, &v)?),
        (Data::Int(v), Scheme::Delta) => {
            let (bases, deltas) = pack::delta_encode(&v);
            Node::IntDelta {
                bases,
                deltas: for_pack(Layout::Interleaved, &deltas)?,
            }
        }
        (Data::Int(v), Scheme::Rle) => {
            let len = v.len();
            let (values, lengths) = runs(&v, |a, b| a == b);
            let values = part(nodes, queue, Data::Int(values), used);
            let lengths = part(nodes, queue, Data::Int(lengths), used);
            Node::IntRle {
                len,
                values,
                lengths,
            }
        }
        (Data::Int(v), Scheme::Dict) => {
            let mut dict: Vec<i64> = v.iter().map(|x| x.cast_signed()).collect();
            dict.sort_unstable();
            dict.dedup();
            let codes = v
                .iter()
                .map(|x| dict.binary_search(&x.cast_signed()).map_or(0, |c| c as u64))
                .collect();
            let dict = part(
                nodes,
                queue,
                Data::Int(dict.into_iter().map(i64::cast_unsigned).collect()),
                used,
            );
            let codes = part(nodes, queue, Data::Int(codes), used);
            Node::IntDict { dict, codes }
        }
        (Data::Int(v), _) => Node::IntRaw(v),
        (Data::Dbl(v), Scheme::One) => Node::DblOne {
            value: v.first().copied().unwrap_or(0.0),
            len: v.len(),
        },
        (Data::Dbl(v), Scheme::Rle) => {
            let len = v.len();
            let (values, lengths) = runs(&v, |a, b| a.to_bits() == b.to_bits());
            let values = part(nodes, queue, Data::Dbl(values), used);
            let lengths = part(nodes, queue, Data::Int(lengths), used);
            Node::DblRle {
                len,
                values,
                lengths,
            }
        }
        (Data::Dbl(v), Scheme::Dict) => {
            let mut dict: Vec<u64> = v.iter().map(|x| x.to_bits()).collect();
            dict.sort_unstable();
            dict.dedup();
            let codes = v
                .iter()
                .map(|x| dict.binary_search(&x.to_bits()).map_or(0, |c| c as u64))
                .collect();
            let dict = part(
                nodes,
                queue,
                Data::Dbl(dict.into_iter().map(f64::from_bits).collect()),
                used,
            );
            let codes = part(nodes, queue, Data::Int(codes), used);
            Node::DblDict { dict, codes }
        }
        (Data::Dbl(v), Scheme::Decimal) => {
            let mut digits = Vec::with_capacity(v.len());
            let mut exponents = Vec::with_capacity(v.len());
            let mut patches = Vec::new();
            for (i, &x) in v.iter().enumerate() {
                match decimal(x) {
                    Some((d, e)) => {
                        digits.push(d.cast_unsigned());
                        exponents.push(e);
                    }
                    None => {
                        // A patch's slot repeats the digits before it, so it costs nothing in runs or widths.
                        digits.push(digits.last().copied().unwrap_or(0));
                        exponents.push(exponents.last().copied().unwrap_or(0));
                        patches.push((i, x));
                    }
                }
            }
            let digits = part(nodes, queue, Data::Int(digits), used);
            let exponents = part(nodes, queue, Data::Int(exponents), used);
            Node::DblDecimal {
                digits,
                exponents,
                patches,
            }
        }
        (Data::Dbl(v), _) => Node::DblRaw(v),
        (Data::Str(s), Scheme::One) => Node::StrOne {
            value: s.get(0).map(<[u8]>::to_vec).unwrap_or_default(),
            len: s.len(),
        },
        (Data::Str(s), Scheme::Dict) => {
            let mut index: HashMap<&[u8], u64> = HashMap::new();
            let mut dict = Strings::new();
            let mut codes = Vec::with_capacity(s.len());
            for entry in s.iter() {
                let entry =
                    entry.ok_or(CodecError::Invalid("string offsets do not cut their bytes"))?;
                let next = dict.len() as u64;
                let code = *index.entry(entry).or_insert_with(|| {
                    dict.push(entry);
                    next
                });
                codes.push(code);
            }
            let dict = part(nodes, queue, Data::Str(dict), used);
            let codes = part(nodes, queue, Data::Int(codes), used);
            Node::StrDict { dict, codes }
        }
        (Data::Str(s), _) => {
            let lengths = s.offsets.windows(2).map(|w| match w {
                [a, b] => b.saturating_sub(*a) as u64,
                _ => 0,
            });
            let lengths = part(nodes, queue, Data::Int(lengths.collect()), used);
            Node::StrRaw {
                bytes: s.bytes,
                lengths,
            }
        }
    })
}

/// Frame of reference: each value's distance from the least (as signed), packed in `layout`.
fn for_pack(layout: Layout, v: &[u64]) -> Result<Packed, CodecError> {
    let base = v
        .iter()
        .map(|x| x.cast_signed())
        .min()
        .unwrap_or(0)
        .cast_unsigned();
    let distances: Vec<u64> = v.iter().map(|x| x.wrapping_sub(base)).collect();
    pack::pack(layout, base, &distances)
}

/// Runs of equal values: each run's value and length.
fn runs<T: Copy>(v: &[T], eq: impl Fn(&T, &T) -> bool) -> (Vec<T>, Vec<u64>) {
    let mut values: Vec<T> = Vec::new();
    let mut lengths: Vec<u64> = Vec::new();
    for x in v {
        match (values.last(), lengths.last_mut()) {
            (Some(last), Some(n)) if eq(last, x) => *n = n.saturating_add(1),
            _ => {
                values.push(*x);
                lengths.push(1);
            }
        }
    }
    (values, lengths)
}

fn is_one(data: &Data) -> bool {
    match data {
        Data::Int(v) => v.windows(2).all(|w| matches!(w, [a, b] if a == b)),
        Data::Dbl(v) => v
            .windows(2)
            .all(|w| matches!(w, [a, b] if a.to_bits() == b.to_bits())),
        Data::Str(s) => {
            let first = s.get(0);
            s.iter().all(|e| e == first)
        }
    }
}

/// `x` as decimal digits and a power of ten, when dividing them gives back `x` bit for bit.
fn decimal(x: f64) -> Option<(i64, u64)> {
    if !x.is_finite() || (x == 0.0 && x.is_sign_negative()) {
        return None;
    }
    for (e, p) in (0u64..).zip(POW10.iter()) {
        if let Some(d) = exact_i64((x * p).round())
            && ((d as f64) / p).to_bits() == x.to_bits()
        {
            return Some((d, e));
        }
    }
    None
}

/// `r` as an integer, when it is one of magnitude under 2^53 (where every integer is a double).
fn exact_i64(r: f64) -> Option<i64> {
    // 2^53: past it a double no longer holds every integer, a format value of IEEE 754 binary64.
    if r.abs() >= 9_007_199_254_740_992.0 || r.is_nan() {
        return None;
    }
    if r == 0.0 {
        return Some(0);
    }
    let bits = r.to_bits();
    let biased = bits.wrapping_shr(52) & 0x7ff;
    let mantissa = (bits & 0x000f_ffff_ffff_ffff) | 0x0010_0000_0000_0000;
    // r = mantissa * 2^(biased - 1075); |r| < 2^53 makes the exponent at most 0.
    let shift = u32::try_from(1075u64.checked_sub(biased)?).ok()?;
    if shift > 52 {
        return None;
    }
    if mantissa & 1u64.wrapping_shl(shift).wrapping_sub(1) != 0 {
        return None;
    }
    let magnitude = i64::try_from(mantissa.wrapping_shr(shift)).ok()?;
    Some(if r.is_sign_negative() {
        magnitude.wrapping_neg()
    } else {
        magnitude
    })
}

// ---------------------------------------------------------------------------------------------
// Choosing a scheme from samples

/// Ranks the schemes on samples that double until the best one stops changing, or the sample is
/// the whole part: so the sample's size follows the data, and no sample size is fixed. The
/// sample is runs of 64 consecutive values (a horizontal group, so runs and deltas inside it are
/// seen), one from each of as many equal strata as there are runs, as BtrBlocks samples runs from
/// across a block.
fn choose(
    len: usize,
    rank: impl Fn(&[Range<usize>]) -> Vec<Scheme>,
) -> Result<Vec<Scheme>, CodecError> {
    let mut count = 1usize;
    let mut best: Option<Scheme> = None;
    loop {
        let whole = count.saturating_mul(GROUP) >= len;
        let ranges: Vec<Range<usize>> = if whole {
            std::iter::once(0..len).collect()
        } else {
            strata(len, count)?
        };
        let ranked = rank(&ranges);
        let first = ranked.first().copied();
        if whole || (best.is_some() && best == first) {
            return Ok(ranked);
        }
        best = first;
        count = count.saturating_mul(2);
    }
}

/// `count` runs of 64, one starting at each of `count` equal strata of `0..len`.
fn strata(len: usize, count: usize) -> Result<Vec<Range<usize>>, CodecError> {
    (0..count)
        .map(|s| {
            let start = s
                .checked_mul(len)
                .and_then(|x| x.checked_div(count))
                .ok_or(CodecError::Overflow)?;
            let end = start.saturating_add(GROUP).min(len);
            Ok(start..end)
        })
        .collect()
}

/// The offered schemes, fewest estimated bits a value first (ties keep the offered order).
fn rank(data: &Data, ranges: &[Range<usize>], offered: &[Scheme]) -> Vec<Scheme> {
    let n: usize = ranges.iter().map(|r| r.len()).sum();
    let mut costs: Vec<(Scheme, f64)> = offered
        .iter()
        .filter_map(|&s| estimate(data, ranges, n, s).map(|bits| (s, bits)))
        .collect();
    costs.sort_by(|a, b| a.1.total_cmp(&b.1));
    costs.into_iter().map(|(s, _)| s).collect()
}

fn bits(n: u64) -> f64 {
    f64::from(width_of(n))
}

/// The bits `count` codes into a dictionary of `distinct` entries take each.
fn code_bits(distinct: usize) -> f64 {
    bits(distinct.saturating_sub(1) as u64)
}

fn span_bits(v: impl Iterator<Item = i64>) -> f64 {
    let (lo, hi) = v.fold((i64::MAX, i64::MIN), |(lo, hi), x| (lo.min(x), hi.max(x)));
    if lo > hi {
        0.0
    } else {
        bits(hi.wrapping_sub(lo).cast_unsigned())
    }
}

/// Estimated bits a value `scheme` leaves on the sample, its parts counted as packed; `None` where
/// it doesn't apply.
fn estimate(data: &Data, ranges: &[Range<usize>], n: usize, scheme: Scheme) -> Option<f64> {
    if n == 0 {
        return Some(0.0);
    }
    let nf = n as f64;
    match data {
        Data::Int(v) => {
            let sample: Vec<&[u64]> = ranges.iter().filter_map(|r| v.get(r.clone())).collect();
            let all = || {
                sample
                    .iter()
                    .flat_map(|s| s.iter().map(|x| x.cast_signed()))
            };
            match scheme {
                Scheme::Raw => Some(64.0),
                Scheme::One => {
                    if one(all()) {
                        Some(0.0)
                    } else {
                        None
                    }
                }
                Scheme::Packed => Some(span_bits(all())),
                Scheme::Delta => {
                    // Row 0's deltas are zero (its values are the bases).
                    let deltas = std::iter::once(0).chain(sample.iter().flat_map(|s| {
                        s.iter()
                            .zip(s.iter().skip(pack::LANES))
                            .map(|(a, b)| b.wrapping_sub(*a).cast_signed())
                    }));
                    // 16 bases of 64 bits each per 1,024 values: 1 bit a value.
                    Some(span_bits(deltas) + 64.0 * (pack::LANES as f64) / (pack::VECTOR as f64))
                }
                Scheme::Rle => {
                    let (vals, lens) = sample_runs(&sample);
                    let r = vals.len() as f64;
                    Some(
                        r * (span_bits(vals.into_iter())
                            + bits(lens.into_iter().max().unwrap_or(0)))
                            / nf,
                    )
                }
                Scheme::Dict => {
                    let mut d: Vec<i64> = all().collect();
                    d.sort_unstable();
                    d.dedup();
                    let dn = d.len();
                    Some(((dn as f64) * span_bits(d.into_iter()) + nf * code_bits(dn)) / nf)
                }
                Scheme::Decimal => None,
            }
        }
        Data::Dbl(v) => {
            let sample: Vec<&[f64]> = ranges.iter().filter_map(|r| v.get(r.clone())).collect();
            let all = || {
                sample
                    .iter()
                    .flat_map(|s| s.iter().map(|x| x.to_bits().cast_signed()))
            };
            match scheme {
                Scheme::Raw => Some(64.0),
                Scheme::One => {
                    if one(all()) {
                        Some(0.0)
                    } else {
                        None
                    }
                }
                Scheme::Rle => {
                    let as_bits: Vec<Vec<u64>> = sample
                        .iter()
                        .map(|s| s.iter().map(|x| x.to_bits()).collect())
                        .collect();
                    let refs: Vec<&[u64]> = as_bits.iter().map(Vec::as_slice).collect();
                    let (vals, lens) = sample_runs(&refs);
                    Some(
                        (vals.len() as f64) * (64.0 + bits(lens.into_iter().max().unwrap_or(0)))
                            / nf,
                    )
                }
                Scheme::Dict => {
                    let mut d: Vec<i64> = all().collect();
                    d.sort_unstable();
                    d.dedup();
                    let dn = d.len();
                    Some(((dn as f64) * 64.0 + nf * code_bits(dn)) / nf)
                }
                Scheme::Decimal => {
                    let mut digits = Vec::with_capacity(n);
                    let mut exps = Vec::with_capacity(n);
                    let mut patches = 0usize;
                    for &x in sample.iter().flat_map(|s| s.iter()) {
                        match decimal(x) {
                            Some((d, e)) => {
                                digits.push(d);
                                exps.push(e.cast_signed());
                            }
                            None => patches = patches.saturating_add(1),
                        }
                    }
                    let patch_bits = (patches as f64) * ((LEN_BYTES + 8) as f64) * 8.0 / nf;
                    Some(span_bits(digits.into_iter()) + span_bits(exps.into_iter()) + patch_bits)
                }
                Scheme::Packed | Scheme::Delta => None,
            }
        }
        Data::Str(s) => {
            let entries = || {
                ranges
                    .iter()
                    .flat_map(|r| r.clone())
                    .map(|i| s.get(i).unwrap_or_default())
            };
            let total_bytes: usize = entries().map(<[u8]>::len).sum();
            let max_len = entries().map(<[u8]>::len).max().unwrap_or(0) as u64;
            match scheme {
                Scheme::Raw => Some(((total_bytes as f64) * 8.0 + nf * bits(max_len)) / nf),
                Scheme::One => {
                    let first = entries().next();
                    if entries().all(|e| Some(e) == first) {
                        Some(0.0)
                    } else {
                        None
                    }
                }
                Scheme::Dict => {
                    let mut d: Vec<&[u8]> = entries().collect();
                    d.sort_unstable();
                    d.dedup();
                    let dn = d.len();
                    let dict_bytes: usize = d.iter().map(|e| e.len()).sum();
                    Some(
                        ((dict_bytes as f64) * 8.0
                            + (dn as f64) * bits(max_len)
                            + nf * code_bits(dn))
                            / nf,
                    )
                }
                _ => None,
            }
        }
    }
}

/// Whether every value is the same.
fn one(mut v: impl Iterator<Item = i64>) -> bool {
    match v.next() {
        None => true,
        Some(first) => v.all(|x| x == first),
    }
}

/// Runs in the sample, each sample run cut at its ends.
fn sample_runs(sample: &[&[u64]]) -> (Vec<i64>, Vec<u64>) {
    let mut vals = Vec::new();
    let mut lens = Vec::new();
    for s in sample {
        let (v, l) = runs(s, |a, b| a == b);
        vals.extend(v.into_iter().map(u64::cast_signed));
        lens.extend(l);
    }
    (vals, lens)
}
