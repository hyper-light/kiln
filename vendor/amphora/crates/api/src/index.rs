//! Index families (CP-T1.5, CP-T1.6): rows of an ordinary family whose keys are an indexed term and
//! then the key of the row they index, written in the same commit as that row, so an index is never
//! apart from what it indexes (kiln's `deploy.release-by-app` and the like, docs/deploy-api.md
//! §14.1). A scan of a term's prefix gives the rows indexed under it, in their keys' order.
//!
//! An index row's key is the term escaped as FoundationDB's tuple layer escapes a byte string (each
//! 0x00 byte as 0x00 0xFF), then the terminator 0x00 0x01, then the indexed key as it is
//! (docs/design/records.md, "Index families"). The escape keeps the terms' bytewise order. Inside
//! an escaped term a 0x00 is always followed by 0xFF, so the terminator can't occur there: no term's
//! prefix is the start of another term's keys, and every index key splits one way only.

use std::collections::BTreeSet;

use crate::{KEY_MAX, Refusal, TX_MAX, Write, bound, family, invalid, key};

const ESCAPE: u8 = 0xFF;
const END: u8 = 0x01;

/// The index family named `family`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    family: String,
}

impl Index {
    pub fn new(family_name: &str) -> Result<Self, Refusal> {
        family(family_name)?;
        Ok(Self {
            family: family_name.into(),
        })
    }

    pub fn family(&self) -> &str {
        &self.family
    }

    /// The scan prefix of every row indexed under `term`.
    pub fn prefix(term: &[u8]) -> Result<Vec<u8>, Refusal> {
        let zeros = term.iter().filter(|b| **b == 0).count();
        let size = term
            .len()
            .checked_add(zeros)
            .and_then(|n| n.checked_add(2))
            .ok_or_else(|| invalid("index term", "size overflow"))?;
        bound("index term", size, KEY_MAX)?;
        let mut out = Vec::with_capacity(size);
        for b in term {
            out.push(*b);
            if *b == 0 {
                out.push(ESCAPE);
            }
        }
        out.extend_from_slice(&[0, END]);
        Ok(out)
    }

    /// The key of the index row for the row at `indexed` under `term`.
    pub fn key(term: &[u8], indexed: &[u8]) -> Result<Vec<u8>, Refusal> {
        key(indexed)?;
        let mut out = Self::prefix(term)?;
        let size = out
            .len()
            .checked_add(indexed.len())
            .ok_or_else(|| invalid("index key", "size overflow"))?;
        bound("index key", size, KEY_MAX)?;
        out.extend_from_slice(indexed);
        Ok(out)
    }

    /// The term and the indexed row's key an index row's key holds.
    pub fn split(index_key: &[u8]) -> Result<(Vec<u8>, Vec<u8>), Refusal> {
        let mut term = Vec::new();
        let mut bytes = index_key.iter().enumerate();
        while let Some((at, b)) = bytes.next() {
            if *b != 0 {
                term.push(*b);
                continue;
            }
            match bytes.next() {
                Some((_, &ESCAPE)) => term.push(0),
                Some((_, &END)) => {
                    let rest = index_key
                        .get(
                            at.checked_add(2)
                                .ok_or_else(|| invalid("index key", "size overflow"))?..,
                        )
                        .ok_or_else(|| invalid("index key", "it ends at its terminator"))?;
                    key(rest)?;
                    return Ok((term, rest.to_vec()));
                }
                _ => {
                    return Err(invalid(
                        "index key",
                        "a 0x00 neither escaped nor ending the term",
                    ));
                }
            }
        }
        Err(invalid("index key", "its term has no terminator"))
    }

    /// The write that indexes the row at `indexed` under `term`.
    pub fn put(&self, term: &[u8], indexed: &[u8]) -> Result<Write, Refusal> {
        Ok(Write::Put {
            family: self.family.clone(),
            key: Self::key(term, indexed)?,
            value: Vec::new(),
        })
    }

    /// The write that takes the row at `indexed` out from under `term`.
    pub fn delete(&self, term: &[u8], indexed: &[u8]) -> Result<Write, Refusal> {
        Ok(Write::Delete {
            family: self.family.clone(),
            key: Self::key(term, indexed)?,
        })
    }

    /// The writes that keep this index in step with the row at `indexed` as its terms go from
    /// `had` to `has`: a delete for each term it no longer has, a put for each it gained, nothing
    /// for a term it kept. They go into the commit that writes the row.
    pub fn moved(
        &self,
        indexed: &[u8],
        had: &[&[u8]],
        has: &[&[u8]],
    ) -> Result<Vec<Write>, Refusal> {
        bound("index terms", had.len().saturating_add(has.len()), TX_MAX)?;
        let had: BTreeSet<&[u8]> = had.iter().copied().collect();
        let has: BTreeSet<&[u8]> = has.iter().copied().collect();
        let mut writes = Vec::new();
        for term in had.difference(&has) {
            writes.push(self.delete(term, indexed)?);
        }
        for term in has.difference(&had) {
            writes.push(self.put(term, indexed)?);
        }
        Ok(writes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every byte string over {0x00, 0x01, 0xFF, b'a'} up to three bytes long.
    fn terms() -> Vec<Vec<u8>> {
        let alphabet = [0x00u8, 0x01, 0xFF, b'a'];
        let mut all = vec![Vec::new()];
        let mut last = vec![Vec::new()];
        for _ in 0..3 {
            let mut next = Vec::new();
            for t in &last {
                for b in alphabet {
                    let mut u = t.clone();
                    u.push(b);
                    next.push(u);
                }
            }
            all.extend(next.iter().cloned());
            last = next;
        }
        all
    }

    /// CP-T1.5: for every pair of small terms (bytes that collide with the escape and the
    /// terminator among them), index keys sort as (term, indexed key) do, and a term's prefix
    /// starts the keys of that term only.
    #[test]
    fn index_keys_keep_term_order_and_prefixes_reach_one_term() {
        let terms = terms();
        let keys: [&[u8]; 3] = [b"\x00", b"\x01x", b"\xff"];
        for a in &terms {
            let pa = Index::prefix(a).unwrap();
            for b in &terms {
                for ka in keys {
                    for kb in keys {
                        let x = Index::key(a, ka).unwrap();
                        let y = Index::key(b, kb).unwrap();
                        assert_eq!(
                            x.cmp(&y),
                            (a, ka).cmp(&(b, kb)),
                            "{a:?}/{ka:?} against {b:?}/{kb:?}"
                        );
                        assert_eq!(
                            y.starts_with(&pa),
                            a == b,
                            "{a:?}'s prefix against {b:?}/{kb:?}"
                        );
                    }
                }
            }
        }
    }

    /// CP-T1.5: an index key splits back into its term and indexed key; a damaged one is refused.
    #[test]
    fn index_keys_split_back_and_damaged_ones_are_refused() {
        for t in terms() {
            for k in [&b"\x00"[..], b"\x00\x01", b"k"] {
                assert_eq!(
                    Index::split(&Index::key(&t, k).unwrap()).unwrap(),
                    (t.clone(), k.to_vec())
                );
            }
        }
        for bad in [&b"ab"[..], b"a\x00", b"a\x00\x02k", b"a\x00\x01"] {
            assert!(
                matches!(Index::split(bad), Err(Refusal::Invalid { .. })),
                "{bad:?}"
            );
        }
    }

    /// CP-T1.5: a term and key over KEY_MAX together are refused TooLarge.
    #[test]
    fn index_keys_are_bounded_as_keys_are() {
        assert!(matches!(
            Index::key(&[b'a'; KEY_MAX], b"k"),
            Err(Refusal::TooLarge { .. })
        ));
        assert!(matches!(
            Index::key(b"t", &[b'k'; KEY_MAX]),
            Err(Refusal::TooLarge { .. })
        ));
        assert!(matches!(
            Index::key(b"t", b""),
            Err(Refusal::Invalid { .. })
        ));
    }

    /// CP-T1.6: moving a row between terms deletes what it left, puts what it gained, and keeps
    /// what it kept untouched.
    #[test]
    fn moved_writes_only_the_difference() {
        let index = Index::new("deploy.release-by-state").unwrap();
        let writes = index
            .moved(b"r1", &[b"queued", b"app"], &[b"running", b"app"])
            .unwrap();
        assert_eq!(
            writes,
            vec![
                index.delete(b"queued", b"r1").unwrap(),
                index.put(b"running", b"r1").unwrap()
            ]
        );
        assert_eq!(
            index.moved(b"r1", &[b"app"], &[b"app"]).unwrap(),
            Vec::new()
        );
    }
}
