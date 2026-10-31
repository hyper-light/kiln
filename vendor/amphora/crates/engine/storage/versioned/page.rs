//! Amphora: a page of the versioned storage's B-link trees (Amphora's `docs/design/engine.md`,
//! decision 3): sorted cells reached through a slot array, its right sibling, and its upper bound.
//!
//! A page covers the keys from the upper bound of the page to its left (or the least key) up to,
//! not including, its own upper bound (`high`, none on the rightmost page of a level): a reader that
//! finds its key at or past a page's bound moves right, which is how readers stay correct while
//! pages split under them (Lehman and Yao). Cells grow down from the page's end, slots up from its
//! header, in key order.
//!
//! Header, little-endian:
//!
//! | bytes  | field                                                           |
//! |--------|-----------------------------------------------------------------|
//! | 0      | kind: 1 a leaf, 2 an inner page                                 |
//! | 1      | level: 0 a leaf, one more for each level of inner pages above   |
//! | 2..4   | cells                                                           |
//! | 4..6   | where the cells begin                                           |
//! | 6..8   | the upper bound's cell (0: none)                                |
//! | 8..16  | the right sibling (0: none)                                     |
//! | 16..24 | an inner page's first child: keys below its first cell's key    |
//!
//! A leaf's cell is a key and a value, an inner page's a key and the child for keys from it, each
//! length a little-endian `u16`; the upper bound is a key alone. A key's length keeps two flags in
//! its top bits (decision 4): the row has versions older than the cell's, kept beside the page, and
//! the cell's version is the row's removal.

use crate::error::LimboError;
use crate::Result;
use std::cmp::Ordering;

/// Bytes of a page.
pub const PAGE_SIZE: usize = 4096;

/// Bytes of the header.
const HEADER: usize = 24;

/// Bytes of a slot.
const SLOT: usize = 2;

/// The largest cell: four fit in a page with their slots, so a page split in two always has room
/// for the cell that split it.
pub const MAX_CELL: usize = (PAGE_SIZE - HEADER) / 4 - SLOT;

/// A key's length's bits; above them, its cell's flags.
const KEY_LEN: u16 = 0x3fff;
const VERSIONED: u16 = 0x8000;
const REMOVED: u16 = 0x4000;

/// A cell's flags.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Flags {
    /// The row has versions older than this cell's, which a snapshot may need: kept beside the
    /// page, by key.
    pub versioned: bool,
    /// This cell's version is the row's removal: the row is absent from it on.
    pub removed: bool,
}

impl Flags {
    fn bits(self) -> u16 {
        (if self.versioned { VERSIONED } else { 0 }) | (if self.removed { REMOVED } else { 0 })
    }

    fn from_bits(b: u16) -> Self {
        Self {
            versioned: b & VERSIONED != 0,
            removed: b & REMOVED != 0,
        }
    }
}

/// What a page holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Keys and their values.
    Leaf,
    /// Keys and the children under them.
    Inner,
}

/// Orders two keys.
pub type Compare<'a> = &'a dyn Fn(&[u8], &[u8]) -> Ordering;

fn corrupt(what: &str) -> LimboError {
    LimboError::Corrupt(format!("versioned page: {what}"))
}

fn get_u16(p: &[u8], at: usize) -> Result<u16> {
    let end = at.checked_add(2).ok_or(LimboError::IntegerOverflow)?;
    let b = p
        .get(at..end)
        .ok_or_else(|| corrupt("a field past the page"))?;
    let b: [u8; 2] = b.try_into().map_err(|_| corrupt("a field's width"))?;
    Ok(u16::from_le_bytes(b))
}

fn set_u16(p: &mut [u8], at: usize, v: u16) -> Result<()> {
    let end = at.checked_add(2).ok_or(LimboError::IntegerOverflow)?;
    p.get_mut(at..end)
        .ok_or_else(|| corrupt("a field past the page"))?
        .copy_from_slice(&v.to_le_bytes());
    Ok(())
}

fn get_u64(p: &[u8], at: usize) -> Result<u64> {
    let end = at.checked_add(8).ok_or(LimboError::IntegerOverflow)?;
    let b = p
        .get(at..end)
        .ok_or_else(|| corrupt("a field past the page"))?;
    let b: [u8; 8] = b.try_into().map_err(|_| corrupt("a field's width"))?;
    Ok(u64::from_le_bytes(b))
}

fn set_u64(p: &mut [u8], at: usize, v: u64) -> Result<()> {
    let end = at.checked_add(8).ok_or(LimboError::IntegerOverflow)?;
    p.get_mut(at..end)
        .ok_or_else(|| corrupt("a field past the page"))?
        .copy_from_slice(&v.to_le_bytes());
    Ok(())
}

fn to_u16(v: usize) -> Result<u16> {
    u16::try_from(v).map_err(|_| LimboError::IntegerOverflow)
}

/// Makes `p` an empty page of `kind`, unbounded, with no right sibling; an inner page's keys below
/// its first cell go to `first`.
pub fn init(p: &mut [u8], kind: Kind, first: u64) -> Result<()> {
    if p.len() != PAGE_SIZE {
        return Err(corrupt("a buffer not a page long"));
    }
    p.fill(0);
    let byte = p.first_mut().ok_or_else(|| corrupt("an empty buffer"))?;
    *byte = match kind {
        Kind::Leaf => 1,
        Kind::Inner => 2,
    };
    set_u16(p, 4, to_u16(PAGE_SIZE)?)?;
    set_u64(p, 16, first)
}

pub fn kind(p: &[u8]) -> Result<Kind> {
    match p.first() {
        Some(1) => Ok(Kind::Leaf),
        Some(2) => Ok(Kind::Inner),
        _ => Err(corrupt("an unknown kind")),
    }
}

pub fn count(p: &[u8]) -> Result<usize> {
    Ok(usize::from(get_u16(p, 2)?))
}

/// The right sibling (none on the rightmost page of its level).
pub fn right(p: &[u8]) -> Result<Option<u64>> {
    let r = get_u64(p, 8)?;
    Ok(if r == 0 { None } else { Some(r) })
}

/// The page's level: 0 a leaf, each level of inner pages one more.
pub fn level(p: &[u8]) -> Result<u8> {
    p.get(1).copied().ok_or_else(|| corrupt("an empty buffer"))
}

pub fn set_level(p: &mut [u8], level: u8) -> Result<()> {
    let byte = p.get_mut(1).ok_or_else(|| corrupt("an empty buffer"))?;
    *byte = level;
    Ok(())
}

pub fn set_right(p: &mut [u8], right: Option<u64>) -> Result<()> {
    set_u64(p, 8, right.unwrap_or(0))
}

/// An inner page's child for keys below its first cell's key.
pub fn first(p: &[u8]) -> Result<u64> {
    get_u64(p, 16)
}

fn content(p: &[u8]) -> Result<usize> {
    Ok(usize::from(get_u16(p, 4)?))
}

fn slot_at(i: usize) -> Result<usize> {
    i.checked_mul(SLOT)
        .and_then(|o| o.checked_add(HEADER))
        .ok_or(LimboError::IntegerOverflow)
}

fn cell_offset(p: &[u8], i: usize) -> Result<usize> {
    if i >= count(p)? {
        return Err(corrupt("a slot past the cells"));
    }
    Ok(usize::from(get_u16(p, slot_at(i)?)?))
}

/// The key of the cell at `at`, and where what follows it begins.
fn key_at(p: &[u8], at: usize) -> Result<(&[u8], usize)> {
    let len = usize::from(get_u16(p, at)? & KEY_LEN);
    let start = at.checked_add(2).ok_or(LimboError::IntegerOverflow)?;
    let end = start.checked_add(len).ok_or(LimboError::IntegerOverflow)?;
    let key = p
        .get(start..end)
        .ok_or_else(|| corrupt("a key past the page"))?;
    Ok((key, end))
}

/// The key of cell `i`.
pub fn key(p: &[u8], i: usize) -> Result<&[u8]> {
    key_at(p, cell_offset(p, i)?).map(|(k, _)| k)
}

/// The flags of cell `i`.
pub fn flags(p: &[u8], i: usize) -> Result<Flags> {
    Ok(Flags::from_bits(get_u16(p, cell_offset(p, i)?)?))
}

/// Sets the flags of cell `i`.
pub fn set_flags(p: &mut [u8], i: usize, flags: Flags) -> Result<()> {
    let at = cell_offset(p, i)?;
    let len = get_u16(p, at)? & KEY_LEN;
    set_u16(p, at, len | flags.bits())
}

/// The value of leaf cell `i`.
pub fn value(p: &[u8], i: usize) -> Result<&[u8]> {
    let (_, at) = key_at(p, cell_offset(p, i)?)?;
    let len = usize::from(get_u16(p, at)?);
    let start = at.checked_add(2).ok_or(LimboError::IntegerOverflow)?;
    let end = start.checked_add(len).ok_or(LimboError::IntegerOverflow)?;
    p.get(start..end)
        .ok_or_else(|| corrupt("a value past the page"))
}

/// The child of inner cell `i`: keys from its key up to the next cell's.
pub fn child(p: &[u8], i: usize) -> Result<u64> {
    let (_, at) = key_at(p, cell_offset(p, i)?)?;
    get_u64(p, at)
}

/// The page's upper bound (none: unbounded).
pub fn high(p: &[u8]) -> Result<Option<&[u8]>> {
    let at = usize::from(get_u16(p, 6)?);
    if at == 0 {
        return Ok(None);
    }
    key_at(p, at).map(|(k, _)| Some(k))
}

/// Whether `key` is at or past the page's upper bound: its place is to the right.
pub fn beyond(p: &[u8], key: &[u8], cmp: Compare) -> Result<bool> {
    Ok(high(p)?.is_some_and(|h| cmp(key, h) != Ordering::Less))
}

/// Where `key` is among the cells (`Ok`), or where it would go (`Err`).
pub fn search(p: &[u8], key: &[u8], cmp: Compare) -> Result<std::result::Result<usize, usize>> {
    let (mut lo, mut hi) = (0usize, count(p)?);
    while lo < hi {
        let mid = lo
            .checked_add(hi.checked_sub(lo).ok_or(LimboError::IntegerOverflow)? / 2)
            .ok_or(LimboError::IntegerOverflow)?;
        match cmp(self::key(p, mid)?, key) {
            Ordering::Less => lo = mid.checked_add(1).ok_or(LimboError::IntegerOverflow)?,
            Ordering::Greater => hi = mid,
            Ordering::Equal => return Ok(Ok(mid)),
        }
    }
    Ok(Err(lo))
}

/// The child of an inner page whose keys include `key`.
pub fn route(p: &[u8], key: &[u8], cmp: Compare) -> Result<u64> {
    match search(p, key, cmp)? {
        Ok(i) => child(p, i),
        Err(0) => first(p),
        Err(i) => child(p, i.checked_sub(1).ok_or(LimboError::IntegerOverflow)?),
    }
}

/// What a cell carries after its key.
#[derive(Clone, Copy, Debug)]
pub enum Tail<'a> {
    /// A leaf's value.
    Value(&'a [u8]),
    /// An inner page's child.
    Child(u64),
    /// Nothing: the upper bound.
    Bound,
}

fn cell_len(key: &[u8], tail: Tail) -> Result<usize> {
    if key.len() > usize::from(KEY_LEN) {
        return Err(LimboError::TooBig);
    }
    let tail = match tail {
        Tail::Value(v) => v.len().checked_add(2).ok_or(LimboError::IntegerOverflow)?,
        Tail::Child(_) => 8,
        Tail::Bound => 0,
    };
    key.len()
        .checked_add(2)
        .and_then(|n| n.checked_add(tail))
        .ok_or(LimboError::IntegerOverflow)
}

/// Whether a cell of `key` and `tail` fits any page: larger rows are refused (`TooBig`).
pub fn fits(key: &[u8], tail: Tail) -> Result<bool> {
    Ok(cell_len(key, tail)? <= MAX_CELL)
}

/// Writes a cell into the free space below the cells; where it begins, or None without room.
fn put_cell(
    p: &mut [u8],
    key: &[u8],
    tail: Tail,
    flags: Flags,
    slots_after: usize,
) -> Result<Option<usize>> {
    let len = cell_len(key, tail)?;
    let top = content(p)?;
    let floor = slot_at(slots_after)?;
    let Some(at) = top.checked_sub(len).filter(|&at| at >= floor) else {
        return Ok(None);
    };
    set_u16(p, at, to_u16(key.len())? | flags.bits())?;
    let key_start = at.checked_add(2).ok_or(LimboError::IntegerOverflow)?;
    let key_end = key_start
        .checked_add(key.len())
        .ok_or(LimboError::IntegerOverflow)?;
    p.get_mut(key_start..key_end)
        .ok_or_else(|| corrupt("a key past the page"))?
        .copy_from_slice(key);
    match tail {
        Tail::Value(v) => {
            set_u16(p, key_end, to_u16(v.len())?)?;
            let start = key_end.checked_add(2).ok_or(LimboError::IntegerOverflow)?;
            let end = start
                .checked_add(v.len())
                .ok_or(LimboError::IntegerOverflow)?;
            p.get_mut(start..end)
                .ok_or_else(|| corrupt("a value past the page"))?
                .copy_from_slice(v);
        }
        Tail::Child(c) => set_u64(p, key_end, c)?,
        Tail::Bound => {}
    }
    set_u16(p, 4, to_u16(at)?)?;
    Ok(Some(at))
}

/// Inserts a cell at slot `at` (cells from `at` on move one slot up): whether it had room, after
/// compacting if what deletions freed was needed.
pub fn insert(p: &mut [u8], at: usize, key: &[u8], tail: Tail) -> Result<bool> {
    insert_flagged(p, at, key, tail, Flags::default())
}

/// [`insert`], the cell with `flags`.
pub fn insert_flagged(
    p: &mut [u8],
    at: usize,
    key: &[u8],
    tail: Tail,
    flags: Flags,
) -> Result<bool> {
    let n = count(p)?;
    if at > n || matches!(tail, Tail::Bound) {
        return Err(corrupt("an insert past the cells"));
    }
    let after = n.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
    let offset = match put_cell(p, key, tail, flags, after)? {
        Some(offset) => offset,
        None => {
            compact(p)?;
            match put_cell(p, key, tail, flags, after)? {
                Some(offset) => offset,
                None => return Ok(false),
            }
        }
    };
    // The slots from `at` move up by one, last first.
    let mut i = n;
    while i > at {
        let prev = i.checked_sub(1).ok_or(LimboError::IntegerOverflow)?;
        let v = get_u16(p, slot_at(prev)?)?;
        set_u16(p, slot_at(i)?, v)?;
        i = prev;
    }
    set_u16(p, slot_at(at)?, to_u16(offset)?)?;
    set_u16(p, 2, to_u16(after)?)?;
    Ok(true)
}

/// Removes the cell at slot `at`; its bytes are reclaimed by the next compaction.
pub fn remove(p: &mut [u8], at: usize) -> Result<()> {
    let n = count(p)?;
    if at >= n {
        return Err(corrupt("a removal past the cells"));
    }
    let last = n.checked_sub(1).ok_or(LimboError::IntegerOverflow)?;
    let mut i = at;
    while i < last {
        let next = i.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
        let v = get_u16(p, slot_at(next)?)?;
        set_u16(p, slot_at(i)?, v)?;
        i = next;
    }
    set_u16(p, 2, to_u16(last)?)
}

/// Replaces leaf cell `at`'s value, keeping its flags: whether it had room.
pub fn replace(p: &mut [u8], at: usize, value: &[u8]) -> Result<bool> {
    let flags = self::flags(p, at)?;
    replace_flagged(p, at, value, flags)
}

/// Replaces leaf cell `at`'s value and flags: whether it had room (without, the cell is as it was).
pub fn replace_flagged(p: &mut [u8], at: usize, value: &[u8], flags: Flags) -> Result<bool> {
    let key = self::key(p, at)?.to_vec();
    let old = self::value(p, at)?.to_vec();
    let old_flags = self::flags(p, at)?;
    remove(p, at)?;
    if insert_flagged(p, at, &key, Tail::Value(value), flags)? {
        return Ok(true);
    }
    // No room for the new value: the old goes back, which had room.
    if !insert_flagged(p, at, &key, Tail::Value(&old), old_flags)? {
        return Err(corrupt("a cell that no longer fits"));
    }
    Ok(false)
}

/// The page's cells, upper bound and header, rewritten with no space between the cells.
pub fn compact(p: &mut [u8]) -> Result<()> {
    let image = p.to_vec();
    rebuild(p, &image)
}

/// Writes `image`'s header, upper bound and cells into `p`, the cells end to end; it touches the
/// cells directly, so nothing it calls compacts again.
fn rebuild(p: &mut [u8], image: &[u8]) -> Result<()> {
    let kind = kind(image)?;
    let n = count(image)?;
    init(p, kind, first(image)?)?;
    set_level(p, level(image)?)?;
    set_right(p, right(image)?)?;
    if let Some(h) = high(image)? {
        let at = put_cell(p, h, Tail::Bound, Flags::default(), n)?
            .ok_or_else(|| corrupt("no room for the bound"))?;
        set_u16(p, 6, to_u16(at)?)?;
    }
    for i in 0..n {
        let k = key(image, i)?;
        let tail = match kind {
            Kind::Leaf => Tail::Value(value(image, i)?),
            Kind::Inner => Tail::Child(child(image, i)?),
        };
        let at = put_cell(p, k, tail, flags(image, i)?, n)?
            .ok_or_else(|| corrupt("cells that no longer fit their page"))?;
        set_u16(p, slot_at(i)?, to_u16(at)?)?;
    }
    set_u16(p, 2, to_u16(n)?)
}

/// Sets the page's upper bound (none: unbounded); it must fit, as a key of a cell that did.
pub fn set_high(p: &mut [u8], high: Option<&[u8]>) -> Result<()> {
    let Some(h) = high else {
        return set_u16(p, 6, 0);
    };
    let n = count(p)?;
    let at = match put_cell(p, h, Tail::Bound, Flags::default(), n)? {
        Some(at) => at,
        None => {
            set_u16(p, 6, 0)?;
            compact(p)?;
            put_cell(p, h, Tail::Bound, Flags::default(), n)?
                .ok_or_else(|| corrupt("no room for the bound"))?
        }
    };
    set_u16(p, 6, to_u16(at)?)
}

/// Splits a full page in two: the upper half of its cells goes to `right`, a fresh page numbered
/// `right_no`, which takes its upper bound and right sibling; the page keeps the lower half, bound
/// by the separator, its right sibling now `right_no`. The separator is returned for the parent:
/// a leaf's is the right page's first key; an inner page's moves up, its child becoming the right
/// page's first.
pub fn split(p: &mut [u8], right: &mut [u8], right_no: u64) -> Result<Vec<u8>> {
    let image = p.to_vec();
    let kind = kind(&image)?;
    let n = count(&image)?;
    if n < 2 {
        return Err(corrupt("a split of fewer than two cells"));
    }
    let mid = n / 2;
    let separator = key(&image, mid)?.to_vec();
    let (right_first, from) = match kind {
        Kind::Leaf => (0, mid),
        Kind::Inner => (
            child(&image, mid)?,
            mid.checked_add(1).ok_or(LimboError::IntegerOverflow)?,
        ),
    };
    init(right, kind, right_first)?;
    set_level(right, level(&image)?)?;
    set_right(right, self::right(&image)?)?;
    set_high(right, high(&image)?)?;
    for (j, i) in (from..n).enumerate() {
        let k = key(&image, i)?;
        let tail = match kind {
            Kind::Leaf => Tail::Value(value(&image, i)?),
            Kind::Inner => Tail::Child(child(&image, i)?),
        };
        if !insert_flagged(right, j, k, tail, flags(&image, i)?)? {
            return Err(corrupt("half a page that doesn't fit one"));
        }
    }
    init(p, kind, first(&image)?)?;
    set_level(p, level(&image)?)?;
    set_right(p, Some(right_no))?;
    set_high(p, Some(&separator))?;
    for i in 0..mid {
        let k = key(&image, i)?;
        let tail = match kind {
            Kind::Leaf => Tail::Value(value(&image, i)?),
            Kind::Inner => Tail::Child(child(&image, i)?),
        };
        if !insert_flagged(p, i, k, tail, flags(&image, i)?)? {
            return Err(corrupt("half a page that doesn't fit one"));
        }
    }
    Ok(separator)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn bytes(a: &[u8], b: &[u8]) -> Ordering {
        a.cmp(b)
    }

    fn leaf() -> Vec<u8> {
        let mut p = vec![0u8; PAGE_SIZE];
        init(&mut p, Kind::Leaf, 0).unwrap();
        p
    }

    fn cells(p: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        (0..count(p).unwrap())
            .map(|i| (key(p, i).unwrap().to_vec(), value(p, i).unwrap().to_vec()))
            .collect()
    }

    /// Do: insert, replace and remove random keys in a leaf, compacting when full. Expect: the
    /// page's cells, in order, are a sorted map's; inserts refused only when the page is full.
    #[test]
    fn a_leaf_keeps_its_cells_as_a_sorted_map_does() {
        let cmp: Compare = &bytes;
        let mut p = leaf();
        let mut model = BTreeMap::new();
        let mut seed = 7u64;
        for _ in 0..5_000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let k = format!("k{:04}", (seed >> 33) % 300).into_bytes();
            let v = vec![b'v'; usize::try_from((seed >> 20) % 40).unwrap()];
            match (seed >> 60) % 3 {
                0 | 1 => match search(&p, &k, cmp).unwrap() {
                    Ok(i) => {
                        if replace(&mut p, i, &v).unwrap() {
                            model.insert(k, v);
                        }
                    }
                    Err(i) => {
                        if insert(&mut p, i, &k, Tail::Value(&v)).unwrap() {
                            model.insert(k, v);
                        } else {
                            assert!(model.len() > 20, "refused with {} cells", model.len());
                        }
                    }
                },
                _ => {
                    if let Ok(i) = search(&p, &k, cmp).unwrap() {
                        remove(&mut p, i).unwrap();
                        model.remove(&k);
                    }
                }
            }
            assert_eq!(cells(&p), model.clone().into_iter().collect::<Vec<_>>());
        }
    }

    /// Do: fill a leaf and split it, then an inner page. Expect: the leaf's cells divided in order
    /// at the separator, the left bound by it and linked to the right, the right taking the old
    /// bound; the inner page's separator moved up, its child the right page's first.
    #[test]
    fn a_split_divides_cells_at_the_separator_and_links_the_halves() {
        let cmp: Compare = &bytes;
        let mut p = leaf();
        set_high(&mut p, Some(b"zz")).unwrap();
        set_right(&mut p, Some(99)).unwrap();
        let mut i = 0;
        while insert(
            &mut p,
            i,
            format!("k{i:04}").as_bytes(),
            Tail::Value(&[1; 30]),
        )
        .unwrap()
        {
            i += 1;
        }
        let all = cells(&p);
        let mut r = vec![0u8; PAGE_SIZE];
        let sep = split(&mut p, &mut r, 5).unwrap();
        let (left, right_cells) = (cells(&p), cells(&r));
        assert_eq!([left.clone(), right_cells.clone()].concat(), all);
        assert_eq!(sep, right_cells[0].0);
        assert_eq!(high(&p).unwrap(), Some(sep.as_slice()));
        assert_eq!(right(&p).unwrap(), Some(5));
        assert_eq!(high(&r).unwrap(), Some(&b"zz"[..]));
        assert_eq!(right(&r).unwrap(), Some(99));
        assert!(beyond(&p, &sep, cmp).unwrap() && !beyond(&p, &left[0].0, cmp).unwrap());

        let mut inner = vec![0u8; PAGE_SIZE];
        init(&mut inner, Kind::Inner, 1).unwrap();
        for c in 0..10u64 {
            let k = format!("s{c:02}");
            insert(
                &mut inner,
                usize::try_from(c).unwrap(),
                k.as_bytes(),
                Tail::Child(10 + c),
            )
            .unwrap();
        }
        let mut r = vec![0u8; PAGE_SIZE];
        let sep = split(&mut inner, &mut r, 6).unwrap();
        assert_eq!(sep, b"s05");
        assert_eq!(first(&r).unwrap(), 15);
        assert_eq!(count(&inner).unwrap() + count(&r).unwrap(), 9);
        assert_eq!(route(&inner, b"s00", cmp).unwrap(), 10);
        assert_eq!(route(&inner, b"a", cmp).unwrap(), 1);
        assert_eq!(route(&r, b"s07", cmp).unwrap(), 17);
    }

    /// Do: flag cells versioned and removed, then remove, compact, replace and split around them.
    /// Expect: each cell keeps its flags through all of it; its key and value unchanged.
    #[test]
    fn flags_stay_with_their_cells_through_compaction_and_splits() {
        let mut p = leaf();
        let flag = |i: usize| Flags {
            versioned: i.is_multiple_of(2),
            removed: i.is_multiple_of(3),
        };
        let mut i = 0;
        while insert_flagged(
            &mut p,
            i,
            format!("k{i:04}").as_bytes(),
            Tail::Value(&[2; 20]),
            flag(i),
        )
        .unwrap()
        {
            i += 1;
        }
        remove(&mut p, 1).unwrap();
        compact(&mut p).unwrap();
        assert!(replace(&mut p, 0, &[3; 10]).unwrap());
        let check = |p: &[u8]| {
            for j in 0..count(p).unwrap() {
                let k = std::str::from_utf8(key(p, j).unwrap()).unwrap();
                let n: usize = k[1..].parse().unwrap();
                assert_eq!(flags(p, j).unwrap(), flag(n), "{k}");
            }
        };
        check(&p);
        let mut r = vec![0u8; PAGE_SIZE];
        split(&mut p, &mut r, 2).unwrap();
        check(&p);
        check(&r);
        set_flags(&mut r, 0, Flags::default()).unwrap();
        assert_eq!(flags(&r, 0).unwrap(), Flags::default());
        assert!(key(&r, 0).unwrap().starts_with(b"k"));
    }

    /// Do: read a page whose header points past its end. Expect: a typed error, no panic.
    #[test]
    fn a_damaged_page_is_refused_as_corrupt() {
        let mut p = leaf();
        insert(&mut p, 0, b"a", Tail::Value(b"b")).unwrap();
        set_u16(&mut p, HEADER, 4095).unwrap();
        assert!(matches!(key(&p, 0), Err(LimboError::Corrupt(_))));
        assert!(matches!(key(&p, 5), Err(LimboError::Corrupt(_))));
    }
}
