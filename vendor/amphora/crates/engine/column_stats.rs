//! What ANALYZE keeps beside `sqlite_stat1` (docs/design/planner.md, decision 20; PL.2): a
//! HyperLogLog sketch of each column's values, for its distinct count, and a reservoir sample of
//! each table's rows. A pure core: ANALYZE's bytecode feeds it a value or a row at a time
//! (`vdbe/execute.rs`, the `AnalyzeSketch` and `AnalyzeSample` aggregates), and `stats.rs` reads
//! what it encoded.
//!
//! Both are the same on every platform (CLAUDE.md §12): the hash is a fixed function of a value's
//! bytes, and the sample's generator is seeded by the table's name, so two passes of one build
//! keep the same statistics and give the same estimates.

use crate::native_ext::{Aggregate, AggregateFunction};
use crate::types::{IOResult, IOResultOr, Value};

/// The sketch's registers: 2^11. Flajolet et al. give the standard error as 1.04 / sqrt(m): 2.3%
/// for m = 2,048, in 2,048 six-bit registers, 1,536 bytes, about Umbra's 1.5 KB a column (Freitag
/// and Neumann, CIDR 2019; docs/research/planner.md f14).
const PRECISION: u8 = 11;
const REGISTERS: usize = 2048;
/// The sketch's relative standard error, 1.04 / sqrt(m) (Flajolet et al.), for its m registers:
/// how far a distinct count read from it may be off (PL.9 reads it as its statistics' error).
pub(crate) fn relative_standard_error() -> f64 {
    1.04 / (REGISTERS as f64).sqrt()
}
/// The hash's bits after a register's index: 64 - 11.
const REST: u32 = 53;
/// Set below the 53 bits a register reads, so their run of leading zeros ends by the 54th. A
/// register, the run plus one, fits six bits.
const GUARD: u64 = 1 << 10;
/// 2,048 registers of six bits.
pub const SKETCH_BYTES: usize = 1536;
/// The sketch's encoding: a version, the precision, the column's non-NULL and NULL values counted
/// (eight bytes each), then the registers packed four in three bytes.
const SKETCH_VERSION: u8 = 1;
const SKETCH_HEADER: usize = 18;
const SKETCH_LEN: usize = SKETCH_HEADER + SKETCH_BYTES;

/// The sample's rows. A value's share of the sample estimates its share of the table within
/// 1.96 * sqrt(p(1 - p) / n) of the truth 95 times in 100: at most 3.1 points at n = 1,024.
pub const SAMPLE_ROWS: usize = 1024;
/// The sample's bytes at most (CLAUDE.md §2): its rows' records. A table whose sample would pass
/// it keeps none, and says so, its estimates falling back to the distinct counts.
pub const SAMPLE_BYTES: usize = 1 << 20;

/// The samples loaded into a database's schema at most, their records' bytes (CLAUDE.md §2):
/// sixty-four tables' samples at their bound. Past it a table's sample is not loaded and its
/// estimates take its distinct counts. A stated bound, to be measured against the planner bench's
/// schemas (JOB's 21 tables, TPC-DS's 24).
pub const LOADED_SAMPLE_BYTES: usize = 64 * SAMPLE_BYTES;

/// A value's sample rows at least, for its share of the sample to stand for its share of the
/// table: below it the count's relative error, about one over its square root, passes a third,
/// and the rows the common values leave, over the values they leave, serve instead.
pub const SAMPLE_HITS: usize = 10;

/// Integers within 2^53 of zero, which a real holds exactly: an integer and a real of the same
/// value compare equal (`1 = 1.0`), so a number there is hashed as the real it equals.
const EXACT_IN_REAL: i64 = 1 << 53;

/// FNV-1a over a tag and bytes, then MurmurHash3's 64-bit finalizer (`fmix64`), which spreads
/// every input bit over the output as HyperLogLog's leading-zero count needs.
fn hash(tag: u8, bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for b in std::iter::once(&tag).chain(bytes) {
        h ^= u64::from(*b);
        h = h.wrapping_mul(PRIME);
    }
    h ^= h.wrapping_shr(33);
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h.wrapping_shr(33);
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ h.wrapping_shr(33)
}

/// The hash of a value, `None` for NULL: a number by the real it equals (its bits, `-0.0` as
/// `0.0`), an integer past 2^53 by its own bytes, text and a BLOB by theirs, each kind apart.
pub fn value_hash(value: &Value) -> Option<u64> {
    let real = |f: f64| {
        hash(
            2,
            &(if f == 0.0 { 0.0f64 } else { f }).to_bits().to_le_bytes(),
        )
    };
    Some(match value {
        Value::Null => return None,
        Value::Numeric(crate::numeric::Numeric::Integer(n)) => {
            if (-EXACT_IN_REAL..=EXACT_IN_REAL).contains(n) {
                real(*n as f64)
            } else {
                hash(1, &n.to_le_bytes())
            }
        }
        Value::Numeric(crate::numeric::Numeric::Float(f)) => real(f64::from(*f)),
        Value::Text(t) => hash(3, t.as_str().as_bytes()),
        Value::Blob(b) => hash(4, b),
    })
}

/// A sketch as ANALYZE builds it: a register a byte, with its counts.
#[derive(Clone)]
pub struct Sketch {
    registers: Box<[u8; REGISTERS]>,
    non_null: u64,
    nulls: u64,
}

impl Default for Sketch {
    fn default() -> Self {
        Self {
            registers: Box::new([0; REGISTERS]),
            non_null: 0,
            nulls: 0,
        }
    }
}

impl Sketch {
    pub fn add(&mut self, value: &Value) {
        let Some(h) = value_hash(value) else {
            self.nulls = self.nulls.saturating_add(1);
            return;
        };
        self.non_null = self.non_null.saturating_add(1);
        self.add_hash(h);
    }

    fn add_hash(&mut self, h: u64) {
        let slot = usize::try_from(h.wrapping_shr(REST)).unwrap_or(0);
        let rest = h.wrapping_shl(u32::from(PRECISION)) | GUARD;
        let rank = u8::try_from(rest.leading_zeros())
            .unwrap_or(u8::MAX)
            .saturating_add(1);
        if let Some(register) = self.registers.get_mut(slot) {
            *register = (*register).max(rank);
        }
    }

    /// The distinct non-NULL values (Flajolet, Fusy, Gandouet and Meunier, "HyperLogLog: the
    /// analysis of a near-optimal cardinality estimation algorithm", AofA 2007, figure 3): the
    /// harmonic mean of the registers, and linear counting where it is under 5m/2 and a register
    /// is still empty. A 64-bit hash needs no large-range correction. Never above the values
    /// counted.
    pub fn distinct(&self) -> f64 {
        let m = REGISTERS as f64;
        let alpha = 0.7213 / (1.0 + 1.079 / m);
        let sum: f64 = self.registers.iter().map(|r| (-f64::from(*r)).exp2()).sum();
        let zeros: usize = self.registers.iter().map(|r| usize::from(*r == 0)).sum();
        let raw = alpha * m * m / sum;
        let estimate = if raw <= 2.5 * m && zeros > 0 {
            m * (m / zeros as f64).ln()
        } else {
            raw
        };
        estimate.min(self.non_null as f64)
    }

    pub fn non_null(&self) -> u64 {
        self.non_null
    }

    pub fn nulls(&self) -> u64 {
        self.nulls
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SKETCH_LEN);
        out.push(SKETCH_VERSION);
        out.push(PRECISION);
        out.extend_from_slice(&self.non_null.to_le_bytes());
        out.extend_from_slice(&self.nulls.to_le_bytes());
        for four in self.registers.as_chunks::<4>().0 {
            let word = four
                .iter()
                .zip([0u32, 6, 12, 18])
                .fold(0u32, |w, (r, at)| w | u32::from(*r & 0x3f).wrapping_shl(at));
            let [a, b, c, _] = word.to_le_bytes();
            out.extend_from_slice(&[a, b, c]);
        }
        out
    }

    /// The sketch `bytes` encodes, `None` for one of another version or precision, or cut short.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (&[version, precision], rest) = bytes.split_first_chunk::<2>()?;
        if version != SKETCH_VERSION || precision != PRECISION {
            return None;
        }
        let (non_null, rest) = rest.split_first_chunk::<8>()?;
        let (nulls, packed) = rest.split_first_chunk::<8>()?;
        if packed.len() != SKETCH_BYTES || bytes.len() != SKETCH_LEN {
            return None;
        }
        let mut sketch = Self {
            non_null: u64::from_le_bytes(*non_null),
            nulls: u64::from_le_bytes(*nulls),
            ..Self::default()
        };
        for (four, three) in sketch
            .registers
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(packed.as_chunks::<3>().0)
        {
            let word = three
                .iter()
                .zip([0u32, 8, 16])
                .fold(0u32, |w, (b, at)| w | u32::from(*b).wrapping_shl(at));
            for (r, at) in four.iter_mut().zip([0u32, 6, 12, 18]) {
                *r = u8::try_from(word.wrapping_shr(at) & 0x3f).unwrap_or(0);
            }
        }
        Some(sketch)
    }
}

/// A reservoir of at most [SAMPLE_ROWS] rows, each the record of a row's columns, chosen
/// uniformly from the rows offered (Vitter, "Random Sampling with a Reservoir", ACM TOMS 11(1),
/// 1985: Algorithm R), by a generator seeded with the table's name.
pub struct Reservoir {
    seen: u64,
    state: u64,
    rows: Vec<Vec<u8>>,
    bytes: usize,
    /// The rows kept passed [SAMPLE_BYTES]: the table keeps no sample.
    over: bool,
}

impl Reservoir {
    pub fn new(table: &str) -> Self {
        Self {
            seen: 0,
            state: hash(5, table.to_ascii_lowercase().as_bytes()),
            rows: Vec::new(),
            bytes: 0,
            over: false,
        }
    }

    /// splitmix64 (Steele, Lea and Flood, OOPSLA 2014).
    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ z.wrapping_shr(30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ z.wrapping_shr(27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ z.wrapping_shr(31)
    }

    /// Offers a row's record: kept in a free place while the reservoir fills, then in place of a
    /// kept row with chance [SAMPLE_ROWS] over the rows offered.
    pub fn offer(&mut self, record: &[u8]) {
        let index = self.seen;
        self.seen = self.seen.saturating_add(1);
        if self.over {
            return;
        }
        let place = if self.rows.len() < SAMPLE_ROWS {
            None
        } else {
            // Uniform over 0..=index by a multiply and shift (Lemire, ACM TOMACS 29(1), 2019).
            let bound = u128::from(index.saturating_add(1));
            let pick = u128::from(self.next())
                .checked_mul(bound)
                .map_or(u128::MAX, |p| p.wrapping_shr(64));
            match usize::try_from(pick) {
                Ok(at) if at < SAMPLE_ROWS => Some(at),
                _ => return,
            }
        };
        let freed = place.and_then(|at| self.rows.get(at)).map_or(0, Vec::len);
        let bytes = self
            .bytes
            .saturating_sub(freed)
            .saturating_add(record.len());
        if bytes > SAMPLE_BYTES {
            self.over = true;
            self.rows = Vec::new();
            self.bytes = 0;
            return;
        }
        self.bytes = bytes;
        match place.and_then(|at| self.rows.get_mut(at)) {
            Some(row) => *row = record.to_vec(),
            None => self.rows.push(record.to_vec()),
        }
    }

    /// The sample's encoding (each row's length, four bytes little-endian, then its record), or
    /// `None` for a table past [SAMPLE_BYTES].
    pub fn encode(&self) -> Option<Vec<u8>> {
        if self.over {
            return None;
        }
        let mut out =
            Vec::with_capacity(self.bytes.saturating_add(self.rows.len().saturating_mul(4)));
        for row in &self.rows {
            out.extend_from_slice(&u32::try_from(row.len()).ok()?.to_le_bytes());
            out.extend_from_slice(row);
        }
        Some(out)
    }
}

/// The records a sample's encoding holds, `None` when it is cut short or holds more than
/// [SAMPLE_ROWS] rows or [SAMPLE_BYTES] bytes.
pub fn decode_sample(mut bytes: &[u8]) -> Option<Vec<&[u8]>> {
    if bytes.len() > SAMPLE_BYTES.saturating_add(SAMPLE_ROWS.saturating_mul(4)) {
        return None;
    }
    let mut rows = Vec::new();
    for _ in 0..SAMPLE_ROWS {
        if bytes.is_empty() {
            return Some(rows);
        }
        let (len, rest) = bytes.split_first_chunk::<4>()?;
        let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
        let (row, rest) = rest.split_at_checked(len)?;
        rows.push(row);
        bytes = rest;
    }
    if bytes.is_empty() {
        Some(rows)
    } else {
        None
    }
}

/// ANALYZE's accumulator of a column's sketch: a value a step, its encoding at the end.
#[derive(Debug, Default)]
pub(crate) struct SketchFunction;

#[derive(Default)]
pub(crate) struct SketchAccumulator(Sketch);

impl std::fmt::Debug for SketchAccumulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SketchAccumulator({} values)", self.0.non_null)
    }
}

impl AggregateFunction for SketchFunction {
    type Accumulator = SketchAccumulator;

    fn create_accumulator(&self) -> crate::Result<Self::Accumulator> {
        Ok(SketchAccumulator::default())
    }
}

impl Aggregate for SketchAccumulator {
    fn step(&mut self, args: &[Value]) -> IOResultOr<()> {
        if let Some(value) = args.first() {
            self.0.add(value);
        }
        Ok(IOResult::Done(()))
    }

    fn finalize(&mut self) -> IOResultOr<Value> {
        Ok(IOResult::Done(Value::Blob(self.0.encode())))
    }
}

/// ANALYZE's accumulator of a table's sample: a row's record a step, the sample's encoding at
/// the end, NULL for a table past [SAMPLE_BYTES].
#[derive(Debug)]
pub(crate) struct SampleFunction {
    pub table: String,
}

pub(crate) struct SampleAccumulator(Reservoir);

impl std::fmt::Debug for SampleAccumulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SampleAccumulator({} rows offered)", self.0.seen)
    }
}

impl AggregateFunction for SampleFunction {
    type Accumulator = SampleAccumulator;

    fn create_accumulator(&self) -> crate::Result<Self::Accumulator> {
        Ok(SampleAccumulator(Reservoir::new(&self.table)))
    }
}

impl Aggregate for SampleAccumulator {
    fn step(&mut self, args: &[Value]) -> IOResultOr<()> {
        if let Some(Value::Blob(record)) = args.first() {
            self.0.offer(record);
        }
        Ok(IOResult::Done(()))
    }

    fn finalize(&mut self) -> IOResultOr<Value> {
        Ok(IOResult::Done(
            self.0.encode().map_or(Value::Null, Value::Blob),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Do: sketch 1 to 1,000,000 distinct integers, each offered three times, with NULLs beside.
    /// Expect: the distinct count within three standard errors (6.9%) at every size, linear
    /// counting's among them (two registers shared give 48.6 for 50), and NULLs counted apart
    /// (T-PL.2.1; decision 20).
    #[test]
    fn a_sketch_counts_distinct_values_within_its_stated_error() {
        for n in [1u64, 7, 50, 99, 1_000, 5_000, 20_000, 134_170, 1_000_000] {
            let mut s = Sketch::default();
            for _ in 0..3 {
                for i in 0..n {
                    s.add(&Value::from_i64(i64::try_from(i).unwrap() * 7919));
                }
            }
            s.add(&Value::Null);
            let d = s.distinct();
            let error = (d - n as f64).abs() / n as f64;
            assert!(error <= 3.0 * 1.04 / (REGISTERS as f64).sqrt(), "{n}: {d}");
            assert_eq!((s.non_null(), s.nulls()), (3 * n, 1));
        }
    }

    /// Do: an integer and the real of its value, text and a BLOB of the same bytes. Expect: the
    /// first two one value, the last two apart, as SQLite compares them (decision 20).
    #[test]
    fn a_value_hashes_as_sqlite_compares_it() {
        assert_eq!(SKETCH_BYTES, REGISTERS * 6 / 8);
        assert_eq!(1usize << PRECISION, REGISTERS);
        assert_eq!(
            value_hash(&Value::from_i64(3)),
            value_hash(&Value::from_f64(3.0))
        );
        assert_eq!(
            value_hash(&Value::from_f64(0.0)),
            value_hash(&Value::from_f64(-0.0))
        );
        assert_ne!(
            value_hash(&Value::from_i64(3)),
            value_hash(&Value::from_f64(3.5))
        );
        assert_ne!(
            value_hash(&Value::build_text("ab")),
            value_hash(&Value::Blob(b"ab".to_vec()))
        );
        assert_eq!(value_hash(&Value::Null), None);
    }

    /// Do: encode a sketch, decode it, and decode a cut and a foreign one. Expect: the same counts
    /// and registers back, in 1,536 bytes of registers; nothing from the others (decision 20).
    #[test]
    fn a_sketch_decodes_to_itself() {
        let mut s = Sketch::default();
        for i in 0..10_000 {
            s.add(&Value::from_i64(i));
        }
        let bytes = s.encode();
        assert_eq!(bytes.len(), SKETCH_HEADER + SKETCH_BYTES);
        let back = Sketch::decode(&bytes).unwrap();
        assert_eq!(back.registers, s.registers);
        assert_eq!(back.distinct(), s.distinct());
        assert!(Sketch::decode(&bytes[..bytes.len() - 1]).is_none());
        let mut other = bytes;
        other[1] = 12;
        assert!(Sketch::decode(&other).is_none());
    }

    /// Do: offer 100,000 rows to a reservoir, twice with one table name and once with another;
    /// then rows past its byte budget. Expect: 1,024 rows kept, the same rows for the same name,
    /// each tenth of the rows offered holding a tenth of the sample within 40 rows, and the table
    /// past the budget keeping none (T-PL.2.1: memory within its bound; decision 20).
    #[test]
    fn a_reservoir_keeps_a_bounded_uniform_sample() {
        let fill = |name: &str| {
            let mut r = Reservoir::new(name);
            for i in 0u32..100_000 {
                r.offer(&i.to_le_bytes());
            }
            r
        };
        let (a, b, c) = (fill("t"), fill("T"), fill("u"));
        assert_eq!(a.rows.len(), SAMPLE_ROWS);
        assert_eq!(a.rows, b.rows);
        assert_ne!(a.rows, c.rows);
        let mut tenths = [0usize; 10];
        for row in &a.rows {
            let i = u32::from_le_bytes(row[..4].try_into().unwrap());
            tenths[(i / 10_000) as usize] += 1;
        }
        for t in tenths {
            assert!(t.abs_diff(SAMPLE_ROWS / 10) <= 40, "{tenths:?}");
        }
        let encoded = a.encode().unwrap();
        assert_eq!(decode_sample(&encoded).unwrap().len(), SAMPLE_ROWS);
        let mut big = Reservoir::new("big");
        for _ in 0..SAMPLE_ROWS {
            big.offer(&[0u8; 2048]);
        }
        assert!(big.encode().is_none());
        assert!(big.bytes <= SAMPLE_BYTES);
    }
}
