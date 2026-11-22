//! A follower's write that read nothing written since its base: read sets (goal ceea4e0a, D on
//! top of the write slot; docs/design/replication.md, "A follower's write that read nothing written
//! since its base: read sets"; docs/research/follower-write-starvation.md).
//!
//! The pager records the pages a prepared session reads (`Pager::record_access`). The read set
//! travels with the write as a trailer after its `Prepared` encoding: an append-only change, since
//! a binary from before it decodes the `Prepared` and ignores what follows (postcard's
//! `from_bytes` reads a value's prefix). The leader, as it appends a member's write prepared on an
//! older base, walks its own log from that base to the entry before the append; if no entry there
//! wrote a page the write read or wrote, it moves the write's base to that entry. Every member then
//! applies it by the exact base, as today, so the decision is the leader's alone, made once, and no
//! member keeps state of its own for it (Kung and Robinson's backward validation over pages,
//! research f4, f5). The window is the leader's log between the base and the append: derived, not
//! picked; a base the log no longer holds is left to the exact base, which refuses it.
//!
//! Page 1 merges its size: an entry that changed page 1 only in the database's size (an
//! allocation) conflicts only with a write that writes page 1 too, never with one that only read
//! it (every session reads page 1); two allocations conflict, as they would allocate the same page
//! (research f9; bench/records/2026-11-07-studio-local2-write-starve-traces).

use amphora_api as api;
use hyper_durable::Point;
use serde::{Deserialize, Serialize};

/// What a write did to page 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Page1 {
    /// It did not write page 1.
    Unwritten,
    /// It changed only the database's size and the checksum trailer: an allocation.
    Size,
    /// It changed more of page 1 (the schema, the free list).
    Other,
}

/// The trailer a write carries after its `Prepared` encoding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReadSet {
    /// The pages its session read, sorted.
    pub(crate) reads: Vec<u32>,
    /// What it did to page 1.
    pub(crate) page1: Page1,
}

/// What an entry in the log wrote, for the leader's walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Wrote {
    /// Resolved pages: their numbers but page 1's, sorted, and what it did to page 1.
    Pages { pages: Vec<u32>, page1: Page1 },
    /// What no page list says (a change of members, a class entry, a seed, an entry it can't
    /// read): nothing prepared before it applies after it.
    Opaque,
}

/// A write as the leader reads it: its base, the pages it wrote, and its read set.
#[derive(Debug)]
pub(crate) struct Write {
    pub(crate) base: Point,
    /// The pages it wrote but page 1, sorted.
    pub(crate) pages: Vec<u32>,
    pub(crate) read: ReadSet,
    /// Where its base's two numbers start in its encoding, and where they end.
    base_at: (usize, usize),
}

/// The head of a `Prepared` encoding, its images borrowed rather than copied.
#[derive(Deserialize)]
struct Head<'a> {
    base_index: u64,
    base_term: u64,
    #[serde(borrow)]
    pages: Vec<(u32, &'a [u8])>,
}

/// The `Prepared` encoding in `data` (past the baseline's prefix and declaration), where it starts,
/// and what follows it.
fn head(data: &[u8]) -> Option<(Head<'_>, usize, &[u8])> {
    // Inside its proposer's stamp (`closed`), which a moved base keeps as it was.
    let (_, inner) = super::closed::open(data).ok()?;
    let body = match inner.strip_prefix(super::BASELINE_DELTA) {
        Some(payload) => {
            let (_, rest) =
                postcard::take_from_bytes::<api::replication::Bootstrap>(payload).ok()?;
            rest
        }
        None => inner,
    };
    let start = data.len().checked_sub(body.len())?;
    let (head, rest) = postcard::take_from_bytes::<Head<'_>>(body).ok()?;
    Some((head, start, rest))
}

fn split_pages(pages: &[(u32, &[u8])], page1: Page1) -> (Vec<u32>, Page1) {
    let mut numbers: Vec<u32> = pages
        .iter()
        .map(|(page, _)| *page)
        .filter(|page| *page != 1)
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    (numbers, page1)
}

impl Write {
    /// The write `data` encodes, if it carries a read set.
    pub(crate) fn decode(data: &[u8]) -> Option<Self> {
        let (head, start, rest) = head(data)?;
        let (read, _) = postcard::take_from_bytes::<ReadSet>(rest).ok()?;
        let (pages, _) = split_pages(&head.pages, read.page1);
        let base = Point {
            index: head.base_index,
            term: head.base_term,
        };
        let after = data.get(start..)?;
        let (_, past) = postcard::take_from_bytes::<(u64, u64)>(after).ok()?;
        let end = data.len().checked_sub(past.len())?;
        Some(Self {
            base,
            pages,
            read,
            base_at: (start, end),
        })
    }

    /// `data`, this write's encoding, with its base moved to `to`.
    pub(crate) fn rebased(&self, data: &[u8], to: Point) -> Option<Vec<u8>> {
        let (start, end) = self.base_at;
        let mut bytes = data.get(..start)?.to_vec();
        bytes.extend(postcard::to_allocvec(&(to.index, to.term)).ok()?);
        bytes.extend_from_slice(data.get(end..)?);
        Some(bytes)
    }

    /// Whether an entry that wrote `wrote` after this write's base, and the pages every apply
    /// writes (`applied`: the replica's marker table's and the closed time's), leaves what this
    /// write read and wrote as it was.
    pub(crate) fn untouched_by(&self, wrote: &Wrote, applied: &[u32]) -> bool {
        let Wrote::Pages { pages, page1 } = wrote else {
            return false;
        };
        let touched = |page: &u32| {
            self.read.reads.binary_search(page).is_ok() || self.pages.binary_search(page).is_ok()
        };
        if applied.iter().any(touched) || pages.iter().any(touched) {
            return false;
        }
        match page1 {
            Page1::Unwritten => true,
            Page1::Size => self.read.page1 == Page1::Unwritten,
            // Every session depends on page 1 beyond its size (the schema and its cookie), whether
            // or not its record caught the read: more than a size change conflicts.
            Page1::Other => false,
        }
    }
}

/// What the normal entry `data` wrote. A `Prepared` with no read set says nothing of page 1 beyond
/// whether it wrote it.
pub(crate) fn wrote(data: &[u8]) -> Wrote {
    if data.is_empty() {
        return Wrote::Pages {
            pages: Vec::new(),
            page1: Page1::Unwritten,
        };
    }
    let Ok((_, inner)) = super::closed::open(data) else {
        return Wrote::Opaque;
    };
    if inner.is_empty() {
        return Wrote::Pages {
            pages: Vec::new(),
            page1: Page1::Unwritten,
        };
    }
    if super::class::is_class(inner) || super::bootstrap::is_seed(inner) {
        return Wrote::Opaque;
    }
    let Some((head, _, rest)) = head(data) else {
        return Wrote::Opaque;
    };
    let page1 = match postcard::take_from_bytes::<ReadSet>(rest) {
        Ok((read, _)) => read.page1,
        Err(_) if head.pages.iter().any(|(page, _)| *page == 1) => Page1::Other,
        Err(_) => Page1::Unwritten,
    };
    let (pages, page1) = split_pages(&head.pages, page1);
    Wrote::Pages { pages, page1 }
}

/// What writing page 1's image `after` over `before` changed: nothing of the header but the
/// database's size (offsets 28 to 31) and the reserved bytes at the page's end (its checksum;
/// header offset 20 says how many) is an allocation (the SQLite file format, "The Database
/// Header": format values, not tuning).
pub(crate) fn page1_change(before: &[u8], after: &[u8]) -> Page1 {
    if before.len() != after.len() {
        return Page1::Other;
    }
    let reserved = before.get(20).copied().map_or(0, usize::from);
    let trailer = before.len().saturating_sub(reserved);
    let only_size = before
        .iter()
        .zip(after)
        .enumerate()
        .all(|(offset, (b, a))| b == a || (28..32).contains(&offset) || offset >= trailer);
    if only_size { Page1::Size } else { Page1::Other }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(reads: &[u32], pages: &[u32], page1: Page1) -> Write {
        Write {
            base: Point { index: 1, term: 1 },
            pages: pages.to_vec(),
            read: ReadSet {
                reads: reads.to_vec(),
                page1,
            },
            base_at: (0, 0),
        }
    }
    fn pages(pages: &[u32], page1: Page1) -> Wrote {
        Wrote::Pages {
            pages: pages.to_vec(),
            page1,
        }
    }

    /// AC-T1.8.R1 (Kung and Robinson's backward validation over pages): do entries that wrote a
    /// page the write read, one it wrote, the marker page, nothing it touched, and an opaque
    /// entry. Expect: only the entry that touched nothing of it leaves it admitted.
    #[test]
    fn r1_a_write_is_untouched_only_by_entries_that_wrote_nothing_it_read_or_wrote() {
        let w = write(&[1, 3, 5], &[5, 9], Page1::Unwritten);
        let marker = 2;
        assert!(w.untouched_by(&pages(&[7, 11], Page1::Unwritten), &[marker]));
        assert!(
            !w.untouched_by(&pages(&[3], Page1::Unwritten), &[marker]),
            "a page read"
        );
        assert!(
            !w.untouched_by(&pages(&[9], Page1::Unwritten), &[marker]),
            "a page written, not read"
        );
        assert!(!w.untouched_by(&Wrote::Opaque, &[marker]));
        let reads_marker = write(&[1, 2], &[2], Page1::Unwritten);
        assert!(
            !reads_marker.untouched_by(&pages(&[], Page1::Unwritten), &[marker]),
            "every apply writes the marker"
        );
    }

    /// AC-T1.8.R1 (research f9): do an entry that changed page 1 only in its size, and one that
    /// changed more of it. Expect: a write that only read page 1 merges with the size change and
    /// conflicts with the other; a write that allocated too (wrote page 1) conflicts with both.
    #[test]
    fn r2_page_ones_size_merges_with_readers_and_conflicts_with_another_allocation() {
        let marker = 2;
        let reader = write(&[1, 3], &[3], Page1::Unwritten);
        assert!(reader.untouched_by(&pages(&[40], Page1::Size), &[marker]));
        assert!(!reader.untouched_by(&pages(&[], Page1::Other), &[marker]));
        let allocates = write(&[1, 3], &[3, 41], Page1::Size);
        assert!(!allocates.untouched_by(&pages(&[40], Page1::Size), &[marker]));
        assert!(!allocates.untouched_by(&pages(&[], Page1::Other), &[marker]));
    }

    /// AC-T1.8.R3 (the append-only wire change): do a `Prepared` with a read set encoded, then read
    /// it as the leader does, rebase it, and decode it as a binary from before read sets does; then
    /// the same inside its proposer's closed-time stamp. Expect: the leader reads its base, pages
    /// and read set; the rebased encoding carries the new base, the same pages and the read set;
    /// the old decoding ignores the trailer; a stamped write moves its base and keeps its stamp.
    #[test]
    fn r3_a_read_set_rides_after_the_prepared_encoding_and_the_base_moves_alone() {
        let prepared = super::super::Prepared::for_test(
            Point { index: 4, term: 2 },
            vec![(1, vec![0; 8]), (6, vec![1; 8])],
            Some(ReadSet {
                reads: vec![1, 6],
                page1: Page1::Size,
            }),
        );
        let data = prepared.encode().unwrap();
        let w = Write::decode(&data).unwrap();
        assert_eq!(w.base, Point { index: 4, term: 2 });
        assert_eq!(w.pages, vec![6]);
        assert_eq!(w.read.reads, vec![1, 6]);
        let moved = w
            .rebased(
                &data,
                Point {
                    index: 300,
                    term: 3,
                },
            )
            .unwrap();
        let again = Write::decode(&moved).unwrap();
        assert_eq!(
            again.base,
            Point {
                index: 300,
                term: 3
            }
        );
        assert_eq!(again.pages, w.pages);
        assert_eq!(again.read, w.read);
        let old: (u64, u64, Vec<(u32, Vec<u8>)>) = postcard::from_bytes(&moved).unwrap();
        assert_eq!((old.0, old.1), (300, 3));
        assert_eq!(old.2, vec![(1, vec![0; 8]), (6, vec![1; 8])]);
        assert_eq!(
            wrote(&moved),
            pages(&[6], Page1::Size),
            "an entry's own read set says what it did to page 1"
        );
        let stamped = super::super::closed::stamp(77, &data);
        let w = Write::decode(&stamped).expect("a stamped write is read inside its stamp");
        assert_eq!(w.base, Point { index: 4, term: 2 });
        let moved = w.rebased(&stamped, Point { index: 9, term: 3 }).unwrap();
        let (stamp, inner) = super::super::closed::open(&moved).unwrap();
        assert_eq!(stamp, Some(77), "the proposer's stamp kept as it was");
        assert_eq!(
            Write::decode(inner).unwrap().base,
            Point { index: 9, term: 3 }
        );
        assert_eq!(wrote(&moved), pages(&[6], Page1::Size));
        let bare = super::super::Prepared::for_test(
            Point { index: 4, term: 2 },
            vec![(1, vec![0; 8])],
            None,
        )
        .encode()
        .unwrap();
        assert!(
            Write::decode(&bare).is_none(),
            "no read set: the exact base decides"
        );
        assert_eq!(wrote(&bare), pages(&[], Page1::Other));
    }

    /// AC-T1.8.R1: do page 1 images that differ in the size, the checksum trailer, and the
    /// schema cookie. Expect: the first two are an allocation; the third is not.
    #[test]
    fn r2_only_the_size_and_checksum_changed_is_an_allocation() {
        let mut before = vec![0u8; 4096];
        before[20] = 8;
        let mut after = before.clone();
        after[31] = 9;
        after[4090] = 1;
        assert_eq!(page1_change(&before, &after), Page1::Size);
        after[43] = 1;
        assert_eq!(page1_change(&before, &after), Page1::Other);
    }
}
