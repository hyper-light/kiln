//! Amphora: key heads for searching inside a B-tree page (G3 step 4, prototype C; PR.4).
//!
//! SQLite's format, Amphora's too, finds a key in a page by binary search over the cell pointer
//! array: each probe follows a pointer into the page and decodes a varint (a table's rowid) or a
//! record header and its first value (an index's key) before it compares. LeanStore keeps a 4-byte
//! normalized head of each key in its slot, truncates the prefix every key in the node shares, and
//! compares heads, touching a key's bytes only when two heads are equal (its
//! `backend/leanstore/storage/btree/core/BTreeNode.hpp:204` and `:357`, at fcb64bf3;
//! docs/research/point-reads.md §3). This module keeps the same heads beside a cached page, in
//! memory, so the file format is unchanged: a [`KeyHeads`] directory for a page holds, for every
//! cell, the 4 bytes of its first key's normalized form that follow the prefix all its cells share.
//!
//! The binary search itself is today's, probe for probe: a head only replaces a probe's compare
//! when it decides it. A head strictly below the key's means the cell's first value orders strictly
//! before the key's first value, so the cell is less whatever its other values and whatever the
//! seek's tie-breaker; strictly above, greater; equal heads leave the probe to today's compare.
//! The cursor visits the same cells, so it finds the same positions and the same equal keys
//! (T-PR.4.1).
//!
//! The normalized form `N` of a value is a byte string whose byte order agrees with the index's:
//! `N(a) < N(b)` implies `a < b`, and `a <= b` implies `N(a) <= N(b)` (equal keys may share a form
//! and different keys may too; neither decides anything). Past its end a form reads as zero bytes.
//! - A table's rowid: its 8 bytes big-endian with the sign bit flipped.
//! - An index's first value: a class byte in SQLite's order of types (NULL 0, numbers 1, text 2,
//!   blobs 3: `ValueRef`'s `Ord`), then for numbers the 8 bytes of the value as an `f64` in total
//!   order (an integer converts to its nearest `f64`, which keeps order and may tie; -0.0 is 0.0),
//!   for text its bytes under the index's collation (BINARY as they are; RTRIM without trailing
//!   spaces; NOCASE lowercased in ASCII and ended at its first NUL, as `nocase_cmp` stops there),
//!   for blobs their bytes. A descending index reverses the order of the forms. Indexes with
//!   another collation (a locale's, a connection's own) or an explicit NULLS FIRST/LAST get no
//!   directory and are searched as today.
//!
//! When a directory is built. The plan says "when a page enters the cache"; built then, a point
//! read of a table larger than the cache would decode every cell of the leaf it reads once to save
//! a binary search's dozen probes. Building is the ski-rental problem: rent (search without it)
//! until the rent paid equals the price, then buy, which never costs more than twice what knowing
//! the future would have (Karlin, Manasse, Rudolph, Sleator, "Competitive Snoopy Caching",
//! Algorithmica 3, 1988; the deterministic break-even rule). Rent and price are counted in the one
//! operation both perform, decoding a cell's first value (a probe's `cell_read_payload_ptr` and
//! compare, a build's `record_norm`), so the rule holds on any machine without a timed constant:
//! - the price is the page's cell count `n`, the decodes a build makes;
//! - each search without the directory is charged the decodes it would have saved: at most the
//!   bit length of `n` (the most probes a binary search over `n` cells makes), times the share of
//!   probes this page's heads decided the last time it had a directory (learned where it runs, and
//!   learned again by each directory; before the page has had one, every probe is assumed
//!   decided, so the first directory is the measurement);
//! - the directory is built once the charges reach `n`.
//!
//! That a build's decode costs what a probe's does is the rule's one assumption; the
//! micro-benchmark measures it (`key_heads_bench`'s `build` mode against its `off` mode).
//!
//! When a directory is dropped. A page's bytes change only while it is dirty (`Pager::add_dirty`
//! marks it before every write), when its buffer is replaced or taken (`set_buffer`,
//! `take_buffer`), when it is renumbered (`set_id`), or when a frame is written over it
//! (`Pager::wal_insert_frame`); each of those clears the page's slot (`KeyHeadsSlot::changed` and
//! `reset`). A directory is neither built
//! nor used while the page is dirty. A search also checks the page's cell count against the
//! directory's, and a probe made against one directory is refused by another.
//!
//! Its memory, derived. A directory holds 4 bytes a cell and a fixed header (the prefix is read from
//! the page, never copied). A page holds at most (page size - 8) / 6 cells: each takes at least 2
//! bytes of cell pointer and 4 of cell (SQLite's `MX_CELL`, `src/btreeInt.h`; Amphora's
//! `MINIMUM_CELL_SIZE`). So a directory is at most 4 * (page size - 8) / 6 bytes plus its header:
//! 2,720 + `size_of::<KeyHeads>()` for a 4,096-byte page, two thirds of the page at worst, and one
//! exists at most for each page the connection's cache holds. Index pages of the suite's text index
//! (cells of about 21 bytes with their pointers) carry a fifth of their size. The slot every cached
//! page carries without a directory is `size_of::<KeyHeadsSlot>()`.
//!
//! Behind a switch, as the plan's prototypes are: `AMPHORA_KEY_HEADS=1` in the environment turns
//! the directory on for every cursor the process opens; off, every search is today's.

use std::cell::Cell;
use std::cmp::Ordering;

use amphora_engine_parser::ast::SortOrder;

use crate::numeric::Numeric;
use crate::storage::btree::PayloadLimits;
use crate::storage::sqlite3_ondisk::{read_text, read_value, read_varint, PageContent, PageType};
use crate::translate::collate::CollationSeq;
use crate::types::{KeyInfo, SerialType, SerialTypeKind, ValueRef};

/// Whether cursors search pages by their key heads: the prototype's switch, read once.
pub(crate) fn switched_on() -> bool {
    static ON: crate::sync::LazyLock<bool> = crate::sync::LazyLock::new(|| {
        std::env::var_os("AMPHORA_KEY_HEADS").is_some_and(|value| value == "1")
    });
    *ON
}

/// Why a page has no directory: it is searched as today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// No cells.
    Empty,
    /// A collation whose order isn't a byte order of the value (a locale's, a connection's own).
    Collation,
    /// NULLS FIRST or LAST, which orders NULL apart from the index's direction.
    NullsOrder,
    /// A head needs bytes of a value that lie on its overflow pages.
    Overflow,
    /// A cell that doesn't decode; today's search reports it.
    Corrupt,
}

/// How a text value orders under the index's collation (`CollationSeq::compare_strings`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fold {
    Binary,
    NoCase,
    Rtrim,
}

/// What a page's cells are ordered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    Rowid,
    Index { fold: Fold, desc: bool },
}

const SIGN: u64 = 0x8000_0000_0000_0000;
const CLASS_NULL: u8 = 0;
const CLASS_NUMBER: u8 = 1;
const CLASS_TEXT: u8 = 2;
const CLASS_BLOB: u8 = 3;
/// The width of a head: LeanStore's 4 bytes (`BTreeNode.hpp:204`, `u32 head`), a format value.
const HEAD_BYTES: usize = 4;

/// A value's normalized form, read byte by byte.
#[derive(Debug, Clone, Copy)]
enum Norm<'a> {
    /// A form of at most 9 bytes: a rowid (8), NULL (its class), a number (class and 8).
    Fixed { bytes: [u8; 9], len: u8 },
    /// Text or a blob: its class, then its bytes. `complete` is false when the bytes are only the
    /// part of a longer value that lies on the page: past them the form is unknown.
    Bytes {
        class: u8,
        bytes: &'a [u8],
        lower: bool,
        complete: bool,
    },
}

impl Norm<'_> {
    /// The byte at `at`: zero past a complete form's end, unknown past an incomplete one's.
    #[inline]
    fn byte(&self, at: usize) -> Option<u8> {
        match self {
            Norm::Fixed { bytes, len } => {
                if at < usize::from(*len) {
                    bytes.get(at).copied()
                } else {
                    Some(0)
                }
            }
            Norm::Bytes {
                class,
                bytes,
                lower,
                complete,
            } => {
                let Some(index) = at.checked_sub(1) else {
                    return Some(*class);
                };
                match bytes.get(index) {
                    Some(byte) if *lower => Some(byte.to_ascii_lowercase()),
                    Some(byte) => Some(*byte),
                    None if *complete => Some(0),
                    None => None,
                }
            }
        }
    }

    /// Where its known bytes end: past it a complete form reads as zero.
    fn len(&self) -> usize {
        match self {
            Norm::Fixed { len, .. } => usize::from(*len),
            Norm::Bytes { bytes, .. } => bytes.len().saturating_add(1),
        }
    }

    /// The head: its 4 bytes from `from`, big-endian, unknown if any of them is.
    #[inline]
    fn head(&self, from: usize) -> Option<u32> {
        let mut head = [0u8; HEAD_BYTES];
        for (offset, slot) in head.iter_mut().enumerate() {
            *slot = self.byte(from.checked_add(offset)?)?;
        }
        Some(u32::from_be_bytes(head))
    }
}

fn rowid_norm<'a>(rowid: i64) -> Norm<'a> {
    let [a, b, c, d, e, f, g, h] = (rowid.cast_unsigned() ^ SIGN).to_be_bytes();
    Norm::Fixed {
        bytes: [a, b, c, d, e, f, g, h, 0],
        len: 8,
    }
}

fn null_norm<'a>() -> Norm<'a> {
    Norm::Fixed {
        bytes: [CLASS_NULL, 0, 0, 0, 0, 0, 0, 0, 0],
        len: 1,
    }
}

/// A number's form: its class, then the `f64`'s bits in total order. Integers and reals compare by
/// value (`Numeric`'s `Ord`, `sqlite_int_float_cmp`); rounding to the nearest `f64` keeps that order
/// (it may only tie), and a value isn't NaN (`NonNan`, and NaN reads as NULL).
fn number_norm<'a>(value: f64) -> Norm<'a> {
    let value = if matches!(value.classify(), std::num::FpCategory::Zero) {
        0.0
    } else {
        value
    };
    let bits = value.to_bits();
    let ordered = if bits & SIGN != 0 { !bits } else { bits | SIGN };
    let [a, b, c, d, e, f, g, h] = ordered.to_be_bytes();
    Norm::Fixed {
        bytes: [CLASS_NUMBER, a, b, c, d, e, f, g, h],
        len: 9,
    }
}

fn text_norm(bytes: &[u8], fold: Fold, complete: bool) -> Norm<'_> {
    match fold {
        Fold::Binary => Norm::Bytes {
            class: CLASS_TEXT,
            bytes,
            lower: false,
            complete,
        },
        // `nocase_cmp` stops at a NUL both sides share: no byte past one orders two keys.
        Fold::NoCase => match bytes.iter().position(|byte| *byte == 0) {
            Some(nul) => Norm::Bytes {
                class: CLASS_TEXT,
                bytes: bytes.get(..nul).unwrap_or_default(),
                lower: true,
                complete: true,
            },
            None => Norm::Bytes {
                class: CLASS_TEXT,
                bytes,
                lower: true,
                complete,
            },
        },
        // Trailing spaces don't order (`rtrim_cmp`). Of a value cut short by its overflow pages,
        // spaces at the end of what is known may or may not trail it: they are unknown.
        Fold::Rtrim => {
            let end = bytes
                .iter()
                .rposition(|byte| *byte != b' ')
                .map_or(0, |last| last.saturating_add(1));
            Norm::Bytes {
                class: CLASS_TEXT,
                bytes: bytes.get(..end).unwrap_or_default(),
                lower: false,
                complete,
            }
        }
    }
}

fn value_norm<'a>(value: &ValueRef<'a>, fold: Fold) -> Norm<'a> {
    match value {
        ValueRef::Null => null_norm(),
        ValueRef::Numeric(Numeric::Integer(integer)) => number_norm(*integer as f64),
        ValueRef::Numeric(Numeric::Float(real)) => number_norm(f64::from(*real)),
        ValueRef::Text(text) => text_norm(text.value.as_bytes(), fold, true),
        ValueRef::Blob(blob) => Norm::Bytes {
            class: CLASS_BLOB,
            bytes: blob,
            lower: false,
            complete: true,
        },
    }
}

/// The form of a record's first value, from the part of its payload on the page (`complete` when
/// that is all of it), and where the value's bytes start in the payload.
fn record_norm(payload: &[u8], complete: bool, fold: Fold) -> Result<(Norm<'_>, usize), Refusal> {
    let (header_size, first_type_at) = read_varint(payload).map_err(|_| Refusal::Corrupt)?;
    let header_size = usize::try_from(header_size).map_err(|_| Refusal::Corrupt)?;
    if first_type_at >= header_size {
        // A record of no values orders by the tie-breaker alone.
        return Err(Refusal::Corrupt);
    }
    let types = payload.get(first_type_at..).ok_or(Refusal::Corrupt)?;
    let (serial_type, _) = read_varint(types).map_err(|_| Refusal::Corrupt)?;
    let serial_type = SerialType::try_from(serial_type).map_err(|_| Refusal::Corrupt)?;
    let data = payload.get(header_size..).ok_or(Refusal::Overflow)?;
    let norm = match serial_type.kind() {
        SerialTypeKind::Text => match data.get(..serial_type.size()) {
            Some(bytes) => {
                // The compare reads the value as UTF-8 and fails on what isn't: so does the build.
                read_text(bytes).map_err(|_| Refusal::Corrupt)?;
                text_norm(bytes, fold, true)
            }
            None if !complete => text_norm(data, fold, false),
            None => return Err(Refusal::Corrupt),
        },
        SerialTypeKind::Blob => match data.get(..serial_type.size()) {
            Some(bytes) => Norm::Bytes {
                class: CLASS_BLOB,
                bytes,
                lower: false,
                complete: true,
            },
            None if !complete => Norm::Bytes {
                class: CLASS_BLOB,
                bytes: data,
                lower: false,
                complete: false,
            },
            None => return Err(Refusal::Corrupt),
        },
        _ => {
            let (value, _) = read_value(data, serial_type).map_err(|_| Refusal::Overflow)?;
            value_norm(&value, fold)
        }
    };
    Ok((norm, header_size))
}

/// Where the shared prefix of a page's forms is read from at a search: copied when it is a fixed
/// form's, read from the page where it is a value's bytes.
#[derive(Debug, Clone, Copy)]
enum Prefix {
    Fixed {
        bytes: [u8; 9],
        len: u8,
    },
    Page {
        class: u8,
        start: usize,
        len: usize,
        lower: bool,
    },
}

/// Where a search key falls against a page's directory, in the order of the forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// Its form is below the prefix every cell shares: every cell is greater.
    BeforeAll,
    /// Above it: every cell is less.
    AfterAll,
    /// It shares the prefix: its head, compared with each cell's.
    Head(u32),
}

/// A search key placed against one page's directory, for the probes of one search of that page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Probe {
    place: Place,
    /// The directory's cell count and prefix: a probe made against one directory is refused by
    /// another.
    cells: usize,
    prefix_len: usize,
}

/// A page's directory of key heads.
#[derive(Debug)]
pub(crate) struct KeyHeads {
    order: Order,
    prefix_len: usize,
    prefix: Prefix,
    heads: Box<[u32]>,
    /// Probes asked of it, and those its heads decided: the share the page's next build is priced
    /// by. Plain counts: a page is searched by one connection at a time.
    asked: Cell<u64>,
    decided: Cell<u64>,
}

impl KeyHeads {
    /// The directory of a table page: its cells' rowids.
    pub(crate) fn build_rowids(contents: &PageContent) -> Result<Self, Refusal> {
        let leaf = match contents.page_type().map_err(|_| Refusal::Corrupt)? {
            PageType::TableLeaf => true,
            PageType::TableInterior => false,
            PageType::IndexLeaf | PageType::IndexInterior => return Err(Refusal::Corrupt),
        };
        let cells = contents.cell_count();
        let mut norms = Vec::new();
        norms
            .try_reserve_exact(cells)
            .map_err(|_| Refusal::Corrupt)?;
        for cell in 0..cells {
            let rowid = if leaf {
                contents.cell_table_leaf_read_rowid(cell)
            } else {
                contents.cell_table_interior_read_rowid(cell)
            }
            .map_err(|_| Refusal::Corrupt)?;
            norms.push(rowid_norm(rowid));
        }
        let first = *norms.first().ok_or(Refusal::Empty)?;
        let Norm::Fixed { bytes, len } = first else {
            return Err(Refusal::Corrupt);
        };
        Self::from_norms(Order::Rowid, &norms, Prefix::Fixed { bytes, len })
    }

    /// The directory of an index page, ordered by `key`, its index's first column.
    pub(crate) fn build_index(
        contents: &PageContent,
        limits: PayloadLimits,
        key: &KeyInfo,
    ) -> Result<Self, Refusal> {
        let order = index_order(key)?;
        let Order::Index { fold, .. } = order else {
            return Err(Refusal::Corrupt);
        };
        let cells = contents.cell_count();
        let mut norms = Vec::new();
        norms
            .try_reserve_exact(cells)
            .map_err(|_| Refusal::Corrupt)?;
        let mut prefix = None;
        for cell in 0..cells {
            let (payload, payload_start, _, overflow) = contents
                .cell_read_payload_at(cell, limits)
                .map_err(|_| Refusal::Corrupt)?;
            let (norm, value_at) = record_norm(payload, overflow.is_none(), fold)?;
            if prefix.is_none() {
                prefix = Some(match norm {
                    Norm::Fixed { bytes, len } => Prefix::Fixed { bytes, len },
                    Norm::Bytes {
                        class,
                        bytes,
                        lower,
                        ..
                    } => Prefix::Page {
                        class,
                        start: payload_start
                            .checked_add(value_at)
                            .ok_or(Refusal::Corrupt)?,
                        len: bytes.len(),
                        lower,
                    },
                });
            }
            norms.push(norm);
        }
        Self::from_norms(order, &norms, prefix.ok_or(Refusal::Empty)?)
    }

    fn from_norms(order: Order, norms: &[Norm<'_>], prefix: Prefix) -> Result<Self, Refusal> {
        let first = norms.first().ok_or(Refusal::Empty)?;
        // The prefix every form shares, compared with the first's: each loop ends at a form's
        // known length, so at most a page's bytes are read.
        let mut prefix_len = usize::MAX;
        for norm in norms {
            let end = first.len().max(norm.len()).min(prefix_len);
            let mut shared = 0;
            while shared < end {
                let mine = first.byte(shared).ok_or(Refusal::Overflow)?;
                let theirs = norm.byte(shared).ok_or(Refusal::Overflow)?;
                if mine != theirs {
                    break;
                }
                shared = shared.saturating_add(1);
            }
            prefix_len = prefix_len.min(shared);
        }
        let mut heads = Vec::new();
        heads
            .try_reserve_exact(norms.len())
            .map_err(|_| Refusal::Corrupt)?;
        for norm in norms {
            heads.push(norm.head(prefix_len).ok_or(Refusal::Overflow)?);
        }
        Ok(Self {
            order,
            prefix_len,
            prefix,
            heads: heads.into_boxed_slice(),
            asked: Cell::new(0),
            decided: Cell::new(0),
        })
    }

    /// How many cells the directory covers.
    pub(crate) fn cells(&self) -> usize {
        self.heads.len()
    }

    /// The prefix's form, its bytes read from `page` when they are a value's.
    fn prefix_norm<'a>(&self, page: &'a [u8]) -> Option<Norm<'a>> {
        Some(match self.prefix {
            Prefix::Fixed { bytes, len } => Norm::Fixed { bytes, len },
            Prefix::Page {
                class,
                start,
                len,
                lower,
            } => Norm::Bytes {
                class,
                bytes: page.get(start..start.checked_add(len)?)?,
                lower,
                complete: true,
            },
        })
    }

    /// Places a key's form against the page's cells.
    fn probe(&self, key: Norm<'_>, page: &[u8]) -> Option<Probe> {
        let prefix = self.prefix_norm(page)?;
        for at in 0..self.prefix_len {
            match key.byte(at)?.cmp(&prefix.byte(at)?) {
                Ordering::Less => return Some(self.probe_at(Place::BeforeAll)),
                Ordering::Greater => return Some(self.probe_at(Place::AfterAll)),
                Ordering::Equal => {}
            }
        }
        Some(self.probe_at(Place::Head(key.head(self.prefix_len)?)))
    }

    fn probe_at(&self, place: Place) -> Probe {
        Probe {
            place,
            cells: self.heads.len(),
            prefix_len: self.prefix_len,
        }
    }

    /// The cell at `cell` against the key `probe` placed, in the page's order, when the heads
    /// decide it: never `Equal`, and `None` when today's compare must.
    #[inline(always)]
    pub(crate) fn decide(&self, probe: Probe, cell: usize) -> Option<Ordering> {
        if probe.cells != self.heads.len() || probe.prefix_len != self.prefix_len {
            return None;
        }
        let head = *self.heads.get(cell)?;
        self.asked.set(self.asked.get().saturating_add(1));
        let forms = match probe.place {
            Place::BeforeAll => Ordering::Greater,
            Place::AfterAll => Ordering::Less,
            Place::Head(key) => match head.cmp(&key) {
                Ordering::Equal => return None,
                decided => decided,
            },
        };
        self.decided.set(self.decided.get().saturating_add(1));
        #[cfg(test)]
        tests::DECIDED.with(|decided| decided.set(decided.get().saturating_add(1)));
        Some(match self.order {
            Order::Index { desc: true, .. } => forms.reverse(),
            Order::Index { desc: false, .. } | Order::Rowid => forms,
        })
    }
}

fn index_order(key: &KeyInfo) -> Result<Order, Refusal> {
    if key.nulls_order.is_some() {
        return Err(Refusal::NullsOrder);
    }
    let fold = match key.collation {
        CollationSeq::Unset | CollationSeq::Binary => Fold::Binary,
        CollationSeq::NoCase => Fold::NoCase,
        CollationSeq::Rtrim => Fold::Rtrim,
        CollationSeq::Locale(_) | CollationSeq::Custom(_) => return Err(Refusal::Collation),
    };
    Ok(Order::Index {
        fold,
        desc: matches!(key.sort_order, SortOrder::Desc),
    })
}

/// The most probes a binary search over `cells` cells makes: the bit length of `cells`.
fn probes(cells: usize) -> usize {
    usize::try_from(usize::BITS.saturating_sub(cells.leading_zeros())).unwrap_or(usize::MAX)
}

#[derive(Debug, Default)]
enum SlotState {
    /// No directory yet: the probes the page's searches were charged since it was cached or last
    /// changed.
    Counting(usize),
    Built(Box<KeyHeads>),
    /// The page can't have one until it changes (why is traced when it's refused).
    Refused,
    #[default]
    Cleared,
}

/// What a cached page keeps for its directory (`PageInner::key_heads`).
#[derive(Debug, Default)]
pub struct KeyHeadsSlot {
    state: SlotState,
    /// The probes asked of the page's last directory and those it decided: what its next build
    /// is priced by. Kept while the page changes in place, forgotten when it leaves the cache.
    last: Option<(u64, u64)>,
}

impl KeyHeadsSlot {
    /// Forgets the directory and what was charged toward one, keeping what the directory
    /// learned: the page's bytes are changing in place (made dirty, a frame written over it).
    #[inline]
    pub fn changed(&mut self) {
        if let SlotState::Built(heads) = &self.state {
            self.last = Some((heads.asked.get(), heads.decided.get()));
        }
        self.state = SlotState::Cleared;
    }

    /// Forgets everything: the page's buffer is replaced or taken, or it is renumbered.
    #[inline]
    pub fn reset(&mut self) {
        self.state = SlotState::Cleared;
        self.last = None;
    }

    /// The page's directory, if built.
    #[inline(always)]
    pub(crate) fn get(&self) -> Option<&KeyHeads> {
        match &self.state {
            SlotState::Built(heads) => Some(heads),
            SlotState::Counting(_) | SlotState::Refused | SlotState::Cleared => None,
        }
    }

    /// Charges a search of the page by `rowid` and places it, once the page has a directory.
    pub(crate) fn probe_rowid(&mut self, contents: &PageContent, rowid: i64) -> Option<Probe> {
        let heads = self.ready(contents, Order::Rowid, || KeyHeads::build_rowids(contents))?;
        heads.probe(rowid_norm(rowid), &[])
    }

    /// Charges a search of the page by a key whose first value is `first` and places it, once the
    /// page has a directory.
    pub(crate) fn probe_index(
        &mut self,
        contents: &PageContent,
        limits: PayloadLimits,
        key: &KeyInfo,
        first: &ValueRef<'_>,
    ) -> Option<Probe> {
        let order = index_order(key).ok()?;
        let Order::Index { fold, .. } = order else {
            return None;
        };
        let heads = self.ready(contents, order, || {
            KeyHeads::build_index(contents, limits, key)
        })?;
        heads.probe(value_norm(first, fold), contents.as_ptr())
    }

    /// The directory, charging this search to the page and building it once the charges reach
    /// its price (the module's ski rental).
    fn ready(
        &mut self,
        contents: &PageContent,
        order: Order,
        build: impl FnOnce() -> Result<KeyHeads, Refusal>,
    ) -> Option<&KeyHeads> {
        let cells = contents.cell_count();
        let charged = match &self.state {
            SlotState::Built(heads) if heads.order == order && heads.cells() == cells => {
                return self.get();
            }
            // Another order or another cell count: a directory for another search, or a page
            // that changed without saying so. Charged again from nothing.
            SlotState::Built(_) | SlotState::Cleared => 0,
            SlotState::Refused => return None,
            SlotState::Counting(charged) => *charged,
        };
        let charged = charged.saturating_add(probes(cells));
        if !pays(charged, cells, self.last) {
            self.state = SlotState::Counting(charged);
            return None;
        }
        self.state = match build() {
            Ok(heads) => SlotState::Built(Box::new(heads)),
            Err(refusal) => {
                tracing::debug!(?refusal, cells, "key heads: page searched as today");
                SlotState::Refused
            }
        };
        self.get()
    }
}

/// Whether `charged` probes, of which the share `last` learned were decided by heads, pay for a
/// build of `cells` decodes: charged * decided / asked >= cells, in integers.
fn pays(charged: usize, cells: usize, last: Option<(u64, u64)>) -> bool {
    let wide = |count: usize| u128::from(u64::try_from(count).unwrap_or(u64::MAX));
    match last {
        Some((asked, decided)) if asked > 0 => {
            wide(charged).saturating_mul(u128::from(decided))
                >= wide(cells).saturating_mul(u128::from(asked))
        }
        _ => charged >= cells,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::num::NonZero;
    use std::sync::Arc;

    use amphora_engine_parser::ast::SortOrder;
    use rand::{Rng, SeedableRng};
    use rand_chacha::ChaCha8Rng;

    use crate::io::{MemoryIO, IO};
    use crate::storage::btree::{BTreeCursor, CursorTrait};
    use crate::translate::collate::CollationSeq;
    use crate::types::{IndexInfo, KeyInfo, SeekKey, SeekOp, Text, Value};
    use crate::util::IOExt;
    use crate::vdbe::Register;
    use crate::{ConnectOpts, Database, SqliteDialect};

    thread_local! {
        /// Probes the heads decided on this thread: the model test's non-vacuity count.
        pub(super) static DECIDED: Cell<u64> = const { Cell::new(0) };
    }

    const OPS: [SeekOp; 6] = [
        SeekOp::GT,
        SeekOp::GE { eq_only: true },
        SeekOp::GE { eq_only: false },
        SeekOp::LE { eq_only: true },
        SeekOp::LE { eq_only: false },
        SeekOp::LT,
    ];

    /// A value of any class, drawn to collide: small integers and the reals equal to them, -0.0,
    /// integers past 2^53, texts sharing prefixes, with NULs, capitals and trailing spaces, long
    /// texts and blobs that spill to overflow pages.
    fn value(rng: &mut ChaCha8Rng) -> Value {
        match rng.random_range(0..10) {
            0 => Value::Null,
            1 => Value::from_i64(rng.random_range(-20..20)),
            2 => Value::from_i64((1i64 << 53) + rng.random_range(-3..3)),
            3 => Value::from_f64(
                [-0.0, 0.0, 0.5, 3.0, -7.0, 9_007_199_254_740_992.0][rng.random_range(0..6)],
            ),
            4 => Value::from_i64(rng.random()),
            5..=7 => {
                let alphabet = [b'a', b'A', b'b', b' ', 0u8, b'z'];
                let mut text: Vec<u8> = b"prefix-"
                    .iter()
                    .copied()
                    .take(rng.random_range(0..8))
                    .collect();
                for _ in 0..rng.random_range(0..10) {
                    text.push(alphabet[rng.random_range(0..alphabet.len())]);
                }
                if rng.random_range(0..20) == 0 {
                    text.extend(std::iter::repeat_n(b'q', 3000));
                    text.push(b'0' + rng.random_range(0..10u8));
                }
                Value::Text(Text::new(String::from_utf8(text).unwrap()))
            }
            _ => {
                let len = if rng.random_range(0..20) == 0 {
                    3000
                } else {
                    rng.random_range(0..6)
                };
                Value::Blob((0..len).map(|_| rng.random_range(0..3u8)).collect())
            }
        }
    }

    fn rootpage(conn: &Arc<crate::Connection>, name: &str) -> i64 {
        let rows = conn
            .prepare(format!(
                "SELECT rootpage FROM sqlite_schema WHERE name = '{name}'"
            ))
            .unwrap()
            .run_collect_rows()
            .unwrap();
        match rows
            .first()
            .and_then(|row| row.first())
            .and_then(Value::as_int)
        {
            Some(root) => root,
            None => panic!("no root page for {name}: {rows:?}"),
        }
    }

    /// T-PR.4.1: random keys of every class through inserts (page splits) and deletes (merges),
    /// each round committed, then seeks of every kind through two cursors on the same pages, one
    /// searching by key heads and one as today: the same result and the same page and cell at
    /// every level, always, and the heads decided probes (not vacuous).
    #[test]
    fn t_pr_4_1_key_heads_find_what_binary_search_finds() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = Database::open_file(io, ":memory:", Arc::new(SqliteDialect)).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, c, d)")
            .unwrap();
        // An index's name, its columns as CREATE INDEX writes them, and each column's order.
        type Index = (&'static str, &'static str, Vec<(SortOrder, CollationSeq)>);
        let indexes: [Index; 5] = [
            ("i_bin", "c", vec![(SortOrder::Asc, CollationSeq::Binary)]),
            (
                "i_desc",
                "c DESC",
                vec![(SortOrder::Desc, CollationSeq::Binary)],
            ),
            (
                "i_nocase",
                "c COLLATE NOCASE",
                vec![(SortOrder::Asc, CollationSeq::NoCase)],
            ),
            (
                "i_rtrim",
                "c COLLATE RTRIM",
                vec![(SortOrder::Asc, CollationSeq::Rtrim)],
            ),
            (
                "i_two",
                "c, d DESC",
                vec![
                    (SortOrder::Asc, CollationSeq::Binary),
                    (SortOrder::Desc, CollationSeq::Binary),
                ],
            ),
        ];
        for (name, columns, _) in &indexes {
            conn.execute(format!("CREATE INDEX {name} ON t ({columns})"))
                .unwrap();
        }
        let mut rng = ChaCha8Rng::seed_from_u64(0x0050_5234);
        let before = DECIDED.with(Cell::get);
        let mut next_id = 1i64;
        for round in 0..6 {
            conn.execute("BEGIN").unwrap();
            for _ in 0..600 {
                let mut stmt = conn
                    .prepare("INSERT INTO t (id, c, d) VALUES (?, ?, ?)")
                    .unwrap();
                stmt.bind_at(NonZero::new(1).unwrap(), Value::from_i64(next_id))
                    .unwrap();
                stmt.bind_at(NonZero::new(2).unwrap(), value(&mut rng))
                    .unwrap();
                stmt.bind_at(NonZero::new(3).unwrap(), value(&mut rng))
                    .unwrap();
                stmt.run_ignore_rows().unwrap();
                next_id += rng.random_range(1..4);
            }
            if round > 0 {
                let from = rng.random_range(1..next_id);
                conn.execute(format!(
                    "DELETE FROM t WHERE id BETWEEN {from} AND {} OR id % 7 = {}",
                    from + 300,
                    round
                ))
                .unwrap();
            }
            conn.execute("COMMIT").unwrap();

            // The root pages, read through the connection before the cursors' read transaction
            // (a statement opens its own; the pager holds one at a time).
            let table_root = rootpage(&conn, "t");
            let index_roots: Vec<i64> = indexes
                .iter()
                .map(|(name, _, _)| rootpage(&conn, name))
                .collect();
            let pager = conn.pager.load().clone();
            pager.begin_read_tx().unwrap();
            // The table, by rowid.
            let root = table_root;
            let mut heads = BTreeCursor::new_table(pager.clone(), root, 3);
            let mut today = BTreeCursor::new_table(pager.clone(), root, 3);
            heads.set_key_heads(true);
            today.set_key_heads(false);
            for _ in 0..3000 {
                let rowid = rng.random_range(-2..next_id + 2);
                let op = OPS[rng.random_range(0..OPS.len())];
                let a = pager
                    .io
                    .block(|| heads.seek(SeekKey::TableRowId(rowid), op))
                    .unwrap();
                let b = pager
                    .io
                    .block(|| today.seek(SeekKey::TableRowId(rowid), op))
                    .unwrap();
                assert_eq!(a, b, "rowid {rowid} {op:?}");
                assert_eq!(heads.position(), today.position(), "rowid {rowid} {op:?}");
            }
            // Each index, by keys of one or two values.
            for ((name, _, key_info), &root) in indexes.iter().zip(&index_roots) {
                let info = Arc::new(
                    IndexInfo::new(
                        key_info.iter().map(|(sort_order, collation)| KeyInfo {
                            sort_order: *sort_order,
                            collation: *collation,
                            nulls_order: None,
                        }),
                        true,
                        key_info.len() + 1,
                        false,
                    )
                    .unwrap(),
                );
                let mut heads = BTreeCursor::new_index_for_test(pager.clone(), root, info.clone());
                let mut today = BTreeCursor::new_index_for_test(pager.clone(), root, info.clone());
                heads.set_key_heads(true);
                today.set_key_heads(false);
                for _ in 0..3000 {
                    let mut key = vec![Register::Value(value(&mut rng))];
                    if key_info.len() > 1 && rng.random_range(0..2) == 0 {
                        key.push(Register::Value(value(&mut rng)));
                    }
                    let op = OPS[rng.random_range(0..OPS.len())];
                    let a = pager.io.block(|| heads.seek_unpacked(&key, op)).unwrap();
                    let b = pager.io.block(|| today.seek_unpacked(&key, op)).unwrap();
                    assert_eq!(a, b, "{name} {key:?} {op:?}");
                    assert_eq!(heads.position(), today.position(), "{name} {key:?} {op:?}");
                }
            }
            pager.end_read_tx();
        }
        assert!(
            DECIDED.with(Cell::get) > before,
            "the key heads decided no probe"
        );
    }
    /// A workload size from the environment, else `default`: the bench's input, not a tuning value.
    fn bench_size(name: &str, default: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    }

    /// The micro-benchmark behind AC-PR.4.1 and the ski rental's one assumption (that a build's
    /// decode costs what a probe's does). Run it, release, nothing else building:
    /// `cargo test -p amphora-engine --release --lib key_heads_bench -- --ignored --nocapture`
    /// (`AMPHORA_KEY_HEADS_BENCH_ROWS`, `AMPHORA_KEY_HEADS_BENCH_SEEKS` size it). It prints, for the
    /// table by rowid and a text index like the suite's:
    /// - `off`: nanoseconds a seek and a probe with today's search (probes counted as the bit
    ///   length of each visited page's cell count, the most a binary search makes);
    /// - `on`: the same with the key heads, directories built by the ski rental, and the share
    ///   of probes the heads decided;
    /// - `build`: nanoseconds a cell decoded building the directories of the pages the seeks
    ///   visited, to set against `off`'s nanoseconds a probe.
    ///
    /// It asserts nothing about time (the seeks it makes are checked equal across modes); the
    /// coordinator records its numbers in `bench/`.
    #[test]
    #[ignore = "micro-benchmark: run on a quiet machine, release"]
    fn key_heads_bench() {
        use std::time::Instant;

        use super::{probes, KeyHeads};
        use crate::storage::btree::PayloadLimits;
        use crate::storage::sqlite3_ondisk::PageType;

        let rows = bench_size("AMPHORA_KEY_HEADS_BENCH_ROWS", 100_000);
        let seeks = bench_size("AMPHORA_KEY_HEADS_BENCH_SEEKS", 200_000);
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = Database::open_file(io, ":memory:", Arc::new(SqliteDialect)).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
            .unwrap();
        conn.execute("CREATE INDEX t_name ON t (name)").unwrap();
        let mut rng = ChaCha8Rng::seed_from_u64(0x0050_5234_4245);
        let mut names = Vec::with_capacity(rows);
        conn.execute("BEGIN").unwrap();
        for id in 0..rows {
            let name = format!("user-{:016x}", rng.random::<u64>());
            let mut stmt = conn
                .prepare("INSERT INTO t (id, name) VALUES (?, ?)")
                .unwrap();
            stmt.bind_at(NonZero::new(1).unwrap(), Value::from_i64(id as i64))
                .unwrap();
            stmt.bind_at(
                NonZero::new(2).unwrap(),
                Value::Text(Text::new(name.clone())),
            )
            .unwrap();
            stmt.run_ignore_rows().unwrap();
            names.push(name);
        }
        conn.execute("COMMIT").unwrap();

        let table_root = rootpage(&conn, "t");
        let index_root = rootpage(&conn, "t_name");
        let pager = conn.pager.load().clone();
        pager.begin_read_tx().unwrap();
        let name_key = KeyInfo {
            sort_order: SortOrder::Asc,
            collation: CollationSeq::Binary,
            nulls_order: None,
        };
        let info = Arc::new(IndexInfo::new([name_key], true, 2, false).unwrap());
        let keys: Vec<usize> = (0..seeks).map(|_| rng.random_range(0..rows)).collect();
        let cell_count = |page: usize| -> usize {
            let (page, _) = pager.io.block(|| pager.read_page(page as i64)).unwrap();
            while page.is_locked() {
                pager.io.step().unwrap();
            }
            page.get_contents().cell_count()
        };

        // Each mode's seeks: nanoseconds a seek, probes a seek, and where each seek stood.
        let run = |index: bool, on: bool| -> (f64, f64, Vec<Vec<(usize, i32)>>) {
            let mut cursor = if index {
                BTreeCursor::new_index_for_test(pager.clone(), index_root, info.clone())
            } else {
                BTreeCursor::new_table(pager.clone(), table_root, 2)
            };
            cursor.set_key_heads(on);
            let op = SeekOp::GE { eq_only: true };
            // A warm pass, so both modes search a cached tree (and `on`'s directories are built).
            for &key in &keys {
                if index {
                    let reg = [Register::Value(Value::Text(Text::new(names[key].clone())))];
                    pager.io.block(|| cursor.seek_unpacked(&reg, op)).unwrap();
                } else {
                    let rowid = key as i64;
                    pager
                        .io
                        .block(|| cursor.seek(SeekKey::TableRowId(rowid), op))
                        .unwrap();
                }
            }
            // The index's keys made before the clock starts.
            let registers: Vec<[Register; 1]> = keys
                .iter()
                .map(|&key| [Register::Value(Value::Text(Text::new(names[key].clone())))])
                .collect();
            let mut positions = Vec::with_capacity(keys.len());
            let started = Instant::now();
            for (&key, reg) in keys.iter().zip(&registers) {
                if index {
                    pager.io.block(|| cursor.seek_unpacked(reg, op)).unwrap();
                } else {
                    let rowid = key as i64;
                    pager
                        .io
                        .block(|| cursor.seek(SeekKey::TableRowId(rowid), op))
                        .unwrap();
                }
                positions.push(cursor.position());
            }
            let elapsed = started.elapsed().as_nanos() as f64;
            let probes: usize = positions
                .iter()
                .flat_map(|levels| levels.iter().map(|(page, _)| probes(cell_count(*page))))
                .sum();
            let per_seek = elapsed / keys.len() as f64;
            (per_seek, probes as f64 / keys.len() as f64, positions)
        };

        let limits = PayloadLimits::new(pager.usable_space());
        for (label, index) in [("table by rowid", false), ("text index", true)] {
            let (off_ns, probes_a_seek, off_at) = run(index, false);
            let decided_before = DECIDED.with(Cell::get);
            let (on_ns, _, on_at) = run(index, true);
            let decided = DECIDED.with(Cell::get) - decided_before;
            assert_eq!(off_at, on_at, "{label}: the modes stood at different cells");
            // `on` ran its seeks twice (warm and timed): the share is over both.
            let decided_share = decided as f64 / (2.0 * probes_a_seek * keys.len() as f64);

            // The directories of the pages the seeks visited, built a round at a time until the
            // cells decoded reach the probes the seeks made.
            let pages: Vec<usize> = {
                let mut seen: Vec<usize> = off_at
                    .iter()
                    .flat_map(|levels| levels.iter().map(|(page, _)| *page))
                    .collect();
                seen.sort_unstable();
                seen.dedup();
                seen
            };
            let loaded: Vec<_> = pages
                .iter()
                .map(|&page| {
                    let (page, _) = pager.io.block(|| pager.read_page(page as i64)).unwrap();
                    while page.is_locked() {
                        pager.io.step().unwrap();
                    }
                    page
                })
                .collect();
            let target = (probes_a_seek * keys.len() as f64) as usize;
            let mut decoded = 0usize;
            let started = Instant::now();
            while decoded < target {
                for page in &loaded {
                    let contents = page.get_contents();
                    let built = match contents.page_type().unwrap() {
                        PageType::TableLeaf | PageType::TableInterior => {
                            KeyHeads::build_rowids(contents)
                        }
                        PageType::IndexLeaf | PageType::IndexInterior => {
                            KeyHeads::build_index(contents, limits, &name_key)
                        }
                    };
                    decoded += std::hint::black_box(built).map_or(0, |heads| heads.cells());
                }
            }
            let build_ns = started.elapsed().as_nanos() as f64 / decoded as f64;
            println!(
                "key_heads_bench {label}: rows {rows} seeks {seeks} probes/seek {probes_a_seek:.2} \
                 | off {off_ns:.1} ns/seek {:.2} ns/probe | on {on_ns:.1} ns/seek {:.2} ns/probe, \
                 decided {:.3} of the most probes | build {build_ns:.2} ns/cell ({} pages)",
                off_ns / probes_a_seek,
                on_ns / probes_a_seek,
                decided_share,
                loaded.len(),
            );
        }
        pager.end_read_tx();
    }
}
