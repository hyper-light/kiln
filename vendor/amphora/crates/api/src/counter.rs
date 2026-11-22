//! Counters (CP-T1.5, CP-T1.6): a number kept in a row of an ordinary family and taken under
//! compare-and-set in the commit that uses it, as kiln's deploy records take release numbers from
//! `deploy.counter` (kiln's docs/deploy-api.md §14.1). Two commits that take from one reading of
//! the row can't both commit: the second is refused `Conflict` and writes nothing, so no number is
//! taken twice, and a commit that doesn't happen takes none.

use std::time::Instant;

use crate::{Cond, Consistency, Records, Refusal, Row, Write, family, invalid, key};

/// A counter row's value: the last number taken, big-endian in eight bytes (a format value: a
/// `u64`'s width, in a byte order fixed across platforms).
pub const WIDTH: usize = 8;

/// The counter at one key of one family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counter {
    family: String,
    key: Vec<u8>,
}

/// A number taken: `cond` and `write` go into the commit that uses `value`, and take it only if
/// that commit commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Taken {
    pub value: u64,
    pub cond: Cond,
    pub write: Write,
}

impl Counter {
    pub fn new(family_name: &str, key_bytes: &[u8]) -> Result<Self, Refusal> {
        family(family_name)?;
        key(key_bytes)?;
        Ok(Self {
            family: family_name.into(),
            key: key_bytes.into(),
        })
    }

    pub fn family(&self) -> &str {
        &self.family
    }

    pub fn key(&self) -> &[u8] {
        &self.key
    }

    /// The last number taken, from the counter's row as read.
    pub fn value(row: &Row) -> Result<u64, Refusal> {
        let bytes: [u8; WIDTH] = row
            .value
            .as_slice()
            .try_into()
            .map_err(|_| invalid("counter row", "its value is not 8 bytes"))?;
        Ok(u64::from_be_bytes(bytes))
    }

    /// The number after the one `current` (this counter's row as read, `None` before the first
    /// is taken) holds: 1 first. Its condition is the row as it was read.
    pub fn after(&self, current: Option<&Row>) -> Result<Taken, Refusal> {
        let (last, cond) = match current {
            None => (
                0,
                Cond::Absent {
                    family: self.family.clone(),
                    key: self.key.clone(),
                },
            ),
            Some(row) if row.key != self.key => {
                return Err(invalid("counter row", "it is another key's row"));
            }
            Some(row) => (
                Self::value(row)?,
                Cond::At {
                    family: self.family.clone(),
                    key: self.key.clone(),
                    rev: row.rev,
                },
            ),
        };
        let value = last
            .checked_add(1)
            .ok_or_else(|| invalid("counter", "every number is taken"))?;
        Ok(Taken {
            value,
            cond,
            write: Write::Put {
                family: self.family.clone(),
                key: self.key.clone(),
                value: value.to_be_bytes().to_vec(),
            },
        })
    }

    /// Reads the counter's row and takes the number after it ([`Counter::after`]).
    pub fn take(
        &self,
        records: &(impl Records + ?Sized),
        until: Instant,
    ) -> Result<Taken, Refusal> {
        let current = records.get(&self.family, &self.key, Consistency::Linearizable, until)?;
        self.after(current.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(value: u64, rev: u64) -> Row {
        Row {
            key: b"n".to_vec(),
            value: value.to_be_bytes().to_vec(),
            rev,
        }
    }

    /// CP-T1.6: the first number is 1, taken where the row is absent; the next is one more than
    /// the row holds, taken at the row's revision.
    #[test]
    fn numbers_follow_the_row_under_its_revision() {
        let counter = Counter::new("deploy.counter", b"n").unwrap();
        let first = counter.after(None).unwrap();
        assert_eq!(first.value, 1);
        assert_eq!(
            first.cond,
            Cond::Absent {
                family: "deploy.counter".into(),
                key: b"n".to_vec()
            }
        );
        let next = counter.after(Some(&row(41, 7))).unwrap();
        assert_eq!(next.value, 42);
        assert_eq!(
            next.cond,
            Cond::At {
                family: "deploy.counter".into(),
                key: b"n".to_vec(),
                rev: 7
            }
        );
        assert_eq!(
            next.write,
            Write::Put {
                family: "deploy.counter".into(),
                key: b"n".to_vec(),
                value: 42u64.to_be_bytes().to_vec()
            }
        );
    }

    /// CP-T1.6: a counter that has taken every number, a row of another width and another key's
    /// row are refused, typed.
    #[test]
    fn exhausted_malformed_and_foreign_rows_are_refused() {
        let counter = Counter::new("deploy.counter", b"n").unwrap();
        assert!(matches!(
            counter.after(Some(&row(u64::MAX, 1))),
            Err(Refusal::Invalid { .. })
        ));
        let short = Row {
            key: b"n".to_vec(),
            value: vec![1, 2, 3],
            rev: 1,
        };
        assert!(matches!(
            counter.after(Some(&short)),
            Err(Refusal::Invalid { .. })
        ));
        let foreign = Row {
            key: b"m".to_vec(),
            ..row(1, 1)
        };
        assert!(matches!(
            counter.after(Some(&foreign)),
            Err(Refusal::Invalid { .. })
        ));
        assert!(matches!(
            Counter::new("Bad!", b"n"),
            Err(Refusal::Invalid { .. })
        ));
        assert!(matches!(
            Counter::new("ok", b""),
            Err(Refusal::Invalid { .. })
        ));
    }
}
