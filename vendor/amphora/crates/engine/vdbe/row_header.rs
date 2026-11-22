//! A B-tree cursor's parse of its current row's record header, kept while the cursor stays on the
//! row, as SQLite's `VdbeCursor` keeps `aType`, `aOffset`, `nHdrParsed` and `iHdrOffset` for
//! `OP_Column` (`src/vdbe.c`): a Column reads where its value starts from here, and parses the
//! header further only past the last column parsed, instead of walking every serial type before
//! its column on every call (docs/design/executor.md, decision 7; QT.3).

use super::{decode_serial_type_into_register, read_serial_type, Register};
use crate::error::LimboError;
use crate::storage::sqlite3_ondisk::read_varint;
use crate::types::get_serial_type_size;
use crate::Result;

/// One column's place in the record: its serial type and where its value starts in the data
/// section.
#[derive(Clone, Copy)]
struct ParsedColumn {
    serial_type: u64,
    offset: usize,
}

/// Where a column's serial type is, once the header is parsed that far.
enum Located {
    Column(ParsedColumn),
    /// The record holds fewer columns than the one asked for: an `ALTER TABLE ... ADD COLUMN`
    /// added it after the row was written, and its default applies.
    PastEnd,
}

/// The parsed prefix of the record header under a cursor. At most `bound` columns are kept, the
/// cursor's column count, so it never holds more than the schema says a row has; a column past
/// that is found by walking on from the last one kept, as before. Dropped (`forget`) wherever the
/// cursor's row changes: a move, a write, a saved position.
pub struct RowHeader {
    columns: Vec<ParsedColumn>,
    bound: usize,
    /// The header's size: where the data section starts in the payload.
    header_end: usize,
    /// Where the serial type after the last kept column starts in the payload.
    next_type_at: usize,
    /// Whether the header's size was read for the row under the cursor.
    started: bool,
}

impl RowHeader {
    /// An empty parse that keeps at most `bound` columns. Allocates nothing until a column is
    /// read.
    pub fn new(bound: usize) -> Self {
        Self {
            columns: Vec::new(),
            bound,
            header_end: 0,
            next_type_at: 0,
            started: false,
        }
    }

    /// Drops the parse: the cursor is on another row, or its row was written.
    #[inline(always)]
    pub fn forget(&mut self) {
        self.columns.clear();
        self.started = false;
    }

    /// Decodes column `column` of `payload`, the record of the row under the cursor, into
    /// `dest`. False when the record holds fewer columns, `dest` untouched.
    #[inline(always)]
    pub fn column_into_register(
        &mut self,
        payload: &[u8],
        column: usize,
        dest: &mut Register,
    ) -> Result<bool> {
        match self.locate(payload, column)? {
            Located::Column(parsed) => {
                let mut data = self.data_at(payload, parsed.offset)?;
                decode_serial_type_into_register(parsed.serial_type, &mut data, dest)?;
                Ok(true)
            }
            Located::PastEnd => Ok(false),
        }
    }

    /// Decodes the columns from `start` on into `dests`, one each in order. Returns how many were
    /// filled, fewer than `dests.len()` when the record holds fewer columns.
    #[inline(always)]
    pub fn columns_into_registers(
        &mut self,
        payload: &[u8],
        start: usize,
        dests: &mut [Register],
    ) -> Result<usize> {
        for (filled, dest) in dests.iter_mut().enumerate() {
            let column = start
                .checked_add(filled)
                .ok_or_else(|| LimboError::InternalError("column index overflows".into()))?;
            if !self.column_into_register(payload, column, dest)? {
                return Ok(filled);
            }
        }
        Ok(dests.len())
    }

    /// The data section of `payload` from `offset` on.
    #[inline(always)]
    fn data_at<'a>(&self, payload: &'a [u8], offset: usize) -> Result<&'a [u8]> {
        payload
            .get(self.header_end..)
            .and_then(|data| data.get(offset..))
            .ok_or_else(too_small)
    }

    /// Parses the header as far as column `column`, keeping what it parses while under the bound.
    #[inline(always)]
    fn locate(&mut self, payload: &[u8], column: usize) -> Result<Located> {
        if let Some(parsed) = self.columns.get(column) {
            return Ok(Located::Column(*parsed));
        }
        self.locate_past_kept(payload, column)
    }

    #[inline(never)]
    fn locate_past_kept(&mut self, payload: &[u8], column: usize) -> Result<Located> {
        if !self.started {
            self.start(payload)?;
        }
        let mut index = self.columns.len();
        let mut type_at = self.next_type_at;
        let mut previous = self.columns.last().copied();
        // Ends at the header's end: each serial type takes at least one of its bytes.
        loop {
            let offset = match previous {
                None => 0,
                Some(previous) => previous
                    .offset
                    .checked_add(get_serial_type_size(previous.serial_type)?)
                    .ok_or_else(too_small)?,
            };
            let mut header = payload
                .get(type_at..self.header_end)
                .ok_or_else(too_small)?;
            if header.is_empty() {
                // The walk this replaces checked the data the skipped columns take even when
                // the record ended before the column asked for.
                if offset > self.data_len(payload) {
                    return Err(too_small());
                }
                return Ok(Located::PastEnd);
            }
            let serial_type = read_serial_type(&mut header)?;
            type_at = self
                .header_end
                .checked_sub(header.len())
                .ok_or_else(header_too_small)?;
            let parsed = ParsedColumn {
                serial_type,
                offset,
            };
            if index == self.columns.len() && self.columns.len() < self.bound {
                if self.columns.capacity() == 0 {
                    self.columns
                        .try_reserve_exact(self.bound)
                        .map_err(|_| LimboError::OutOfMemory)?;
                }
                self.columns.push(parsed);
                self.next_type_at = type_at;
            }
            if index == column {
                if offset > self.data_len(payload) {
                    return Err(too_small());
                }
                return Ok(Located::Column(parsed));
            }
            previous = Some(parsed);
            index = index.checked_add(1).ok_or_else(header_too_small)?;
        }
    }

    /// Reads the header's size, with the checks `ValueIterator::new` makes.
    fn start(&mut self, payload: &[u8]) -> Result<()> {
        let (header_size, varint_len) = read_varint(payload)?;
        let header_size = usize::try_from(header_size).map_err(|_| header_too_small())?;
        if header_size > payload.len() || varint_len > payload.len() || varint_len > header_size {
            return Err(header_too_small());
        }
        self.header_end = header_size;
        self.next_type_at = varint_len;
        self.started = true;
        Ok(())
    }

    fn data_len(&self, payload: &[u8]) -> usize {
        payload.len().saturating_sub(self.header_end)
    }
}

fn too_small() -> LimboError {
    LimboError::Corrupt("Data section too small for indicated serial type size".into())
}

fn header_too_small() -> LimboError {
    LimboError::Corrupt("Payload too small for indicated header size".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ImmutableRecord, Value, ValueIterator};
    use crate::vdbe::ValueIteratorExt;
    use rand::{Rng, SeedableRng};
    use rand_chacha::ChaCha8Rng;

    /// A value of every storage class and integer width: NULL, 8- to 64-bit integers, 0 and 1
    /// (serial types 8 and 9), REAL, empty and long TEXT and BLOB.
    fn random_value(rng: &mut ChaCha8Rng) -> Value {
        match rng.random_range(0..12) {
            0 => Value::Null,
            1 => Value::from_i64(0),
            2 => Value::from_i64(1),
            3 => Value::from_i64(rng.random_range(-128..128)),
            4 => Value::from_i64(rng.random_range(-32_768..32_768)),
            5 => Value::from_i64(rng.random_range(-8_388_608..8_388_608)),
            6 => Value::from_i64(rng.random_range(i64::from(i32::MIN)..=i64::from(i32::MAX))),
            7 => Value::from_i64(rng.random_range(-(1i64 << 47)..(1i64 << 47))),
            8 => Value::from_i64(rng.random()),
            9 => Value::from_f64(rng.random_range(-1e12..1e12)),
            10 => {
                let len = rng.random_range(0..300);
                Value::build_text(
                    (0..len)
                        .map(|i| char::from(b'a' + (i % 26) as u8))
                        .collect::<String>(),
                )
            }
            _ => {
                let len = rng.random_range(0..300);
                let bytes: Vec<u8> = (0..len).map(|_| rng.random()).collect();
                Value::from_blob(crate::types::value_blob_from_slice(&bytes).unwrap())
            }
        }
    }

    fn walk(payload: &[u8], column: usize) -> Option<Value> {
        let mut dest = Register::Value(Value::Null);
        ValueIterator::new(payload)
            .unwrap()
            .nth_into_register(column, &mut dest)
            .map(|result| {
                result.unwrap();
                dest.get_value().clone()
            })
    }

    /// Do: build random records (every serial type, NULLs, long TEXT and BLOB) and read their
    /// columns in a random order, some twice and some past the record's end, through the parse
    /// and through the header walk it replaces. Expect: the same value, or the same "past the
    /// end", every time (T-QT.3.1).
    #[test]
    fn a_parsed_header_reads_every_column_as_the_walk_does() {
        let mut rng = ChaCha8Rng::seed_from_u64(0x7788_ef29);
        for _ in 0..2_000 {
            let width = rng.random_range(1..24);
            let values: Vec<Value> = (0..width).map(|_| random_value(&mut rng)).collect();
            let record = ImmutableRecord::from_values(&values, values.len()).unwrap();
            let payload = record.get_payload();
            // The schema may name more columns than the record holds, or fewer.
            let bound = rng.random_range(0..width + 4);
            let mut header = RowHeader::new(bound);
            for _ in 0..width * 2 {
                let column = rng.random_range(0..width + 3);
                let mut dest = Register::Value(Value::Null);
                let found = header
                    .column_into_register(payload, column, &mut dest)
                    .unwrap();
                match walk(payload, column) {
                    Some(want) => {
                        assert!(found, "column {column} of {width}");
                        assert_eq!(dest.get_value(), &want, "column {column} of {width}");
                    }
                    None => assert!(!found, "column {column} past {width}"),
                }
            }
            assert!(header.columns.len() <= bound, "kept past the bound");
        }
    }

    /// Do: read a run of columns from a random start into registers, through the parse and
    /// column by column through the walk. Expect: the same values and the same count filled
    /// (T-QT.3.1).
    #[test]
    fn a_parsed_header_reads_a_run_of_columns_as_the_walk_does() {
        let mut rng = ChaCha8Rng::seed_from_u64(0x0003);
        for _ in 0..1_000 {
            let width = rng.random_range(1..20);
            let values: Vec<Value> = (0..width).map(|_| random_value(&mut rng)).collect();
            let record = ImmutableRecord::from_values(&values, values.len()).unwrap();
            let payload = record.get_payload();
            let mut header = RowHeader::new(width);
            // A column read first, as a filter would, then the run.
            let first = rng.random_range(0..width);
            let mut scratch = Register::Value(Value::Null);
            header
                .column_into_register(payload, first, &mut scratch)
                .unwrap();
            let start = rng.random_range(0..width + 2);
            let count = rng.random_range(1..width + 3);
            let mut dests: Vec<Register> =
                (0..count).map(|_| Register::Value(Value::Null)).collect();
            let filled = header
                .columns_into_registers(payload, start, &mut dests)
                .unwrap();
            let want: Vec<Value> = (start..start + count)
                .map_while(|column| walk(payload, column))
                .collect();
            assert_eq!(filled, want.len());
            for (dest, want) in dests.iter().zip(&want) {
                assert_eq!(dest.get_value(), want);
            }
        }
    }

    /// Do: read a column of one record, forget, then read the same column of a record whose
    /// columns sit elsewhere. Expect: the second record's value; without the forget, the first
    /// record's offsets would read the wrong bytes (T-QT.3.2).
    #[test]
    fn a_forgotten_header_reads_the_next_row_afresh() {
        let first = ImmutableRecord::from_values(
            &[
                Value::from_i64(1),
                Value::build_text("a"),
                Value::from_i64(7),
            ],
            3,
        )
        .unwrap();
        let second = ImmutableRecord::from_values(
            &[
                Value::from_i64(1 << 40),
                Value::build_text("a much longer text"),
                Value::from_i64(9),
            ],
            3,
        )
        .unwrap();
        let mut header = RowHeader::new(3);
        let mut dest = Register::Value(Value::Null);
        assert!(header
            .column_into_register(first.get_payload(), 2, &mut dest)
            .unwrap());
        assert_eq!(dest.get_value(), &Value::from_i64(7));
        header.forget();
        assert!(header
            .column_into_register(second.get_payload(), 2, &mut dest)
            .unwrap());
        assert_eq!(dest.get_value(), &Value::from_i64(9));
    }

    /// Do: read a column of a record whose header names more data than the record holds, and of
    /// one whose header size runs past its end. Expect: the walk's Corrupt errors, not a panic
    /// (hostile input, CLAUDE.md §6).
    #[test]
    fn a_corrupt_header_is_refused_as_the_walk_refuses_it() {
        // Header of 3 bytes: an 8-byte integer, then a 4-byte one; 2 data bytes.
        let short = [3, 6, 4, 0, 0];
        let mut dest = Register::Value(Value::Null);
        for column in 0..3 {
            let walked = ValueIterator::new(&short)
                .unwrap()
                .nth_into_register(column, &mut dest)
                .map(|r| r.map_err(|e| e.to_string()));
            let parsed = RowHeader::new(4)
                .column_into_register(&short, column, &mut dest)
                .map_err(|e| e.to_string());
            match (walked, parsed) {
                (Some(Err(want)), Err(got)) => assert_eq!(got, want, "column {column}"),
                (None, Err(got)) => assert!(got.contains("Data section too small"), "{got}"),
                (walked, parsed) => panic!("column {column}: walk {walked:?}, parse {parsed:?}"),
            }
        }
        let header_past_end = [9, 1];
        assert!(RowHeader::new(2)
            .column_into_register(&header_past_end, 0, &mut dest)
            .is_err());
        assert!(RowHeader::new(2)
            .column_into_register(&[], 0, &mut dest)
            .is_err());
    }
}
