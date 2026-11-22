//! Amphora (query speed step 7, QT.7; G5 step 2): a table scan that decodes each leaf once into
//! column batches.
//!
//! When the connection's switch is on (`Connection::set_vectorized_scans`), a read-only program's
//! scanned table cursors (opened, rewound and stepped, never sought) read their columns from a
//! batch: the cells of the leaf under the cursor, from the cell it stands on onward (backward
//! for a descending scan), decoded together, each record header parsed once, into one vector per
//! column the program reads. Every `Column` on the cursor's rows then copies its value out of
//! the batch, so the filters and aggregates after it consume the leaf batch by batch, through
//! the VDBE's one evaluator of each expression (CLAUDE.md §13: no second copy of SQL's
//! semantics). The record format is unchanged.
//!
//! The batch size is derived where the scan runs, never picked: a batch holds as many rows as fit
//! in the smaller of the core's measured second-level data cache (Boncz et al., MonetDB/X100,
//! CIDR 2005: vectors that stay in cache; Kersten et al., VLDB 2018: the vector working set must
//! fit the cache) and the statement's memory budget shared among its batched cursors, each row
//! charged its own measured size (its record's bytes on the page, an upper bound of its decoded
//! text and blob bytes, plus one value slot per column read). Its stated fallback: a row the
//! budget can't hold, or whose record spills to overflow pages, is read row-at-a-time as before;
//! a platform whose cache size can't be read (`BatchRefusal::CacheSizeUnknown`) scans
//! row-at-a-time.
//!
//! COUNT(*) over a whole table is answered from the leaves' cell counts without decoding a row,
//! by the `Count` instruction (`translate/select.rs`, `BTreeCursor::count`), switch on or off.

use crate::alloc::TryClone;
use crate::error::LimboError;
use crate::storage::btree::{BTreeCursor, TableLeafUnderCursor};
use crate::vdbe::builder::{CursorKey, CursorType};
use crate::vdbe::insn::Insn;
use crate::vdbe::row_header::RowHeader;
use crate::vdbe::Register;
use crate::{Result, Value};

/// The scanned table cursors of a program and the columns it reads from each, found once when
/// the program is built.
#[derive(Debug, Clone, Default)]
pub struct BatchPlan {
    /// By cursor id: the columns the program reads from it, ascending, or None when it isn't a
    /// scanned table cursor.
    columns: Vec<Option<Box<[usize]>>>,
    /// How many cursors share the statement's memory budget.
    batched_cursors: usize,
    /// The scan loops whose bodies run over a whole batch, by their Next's address
    /// (`leaf_batch/fused.rs`), ascending.
    fused: Vec<fused::FusedLoop>,
}

impl BatchPlan {
    /// The plan for a read-only program, or None when no cursor qualifies. A table cursor
    /// qualifies when the program rewinds it (or moves it to its last row) and never seeks it:
    /// a scan walks a leaf's cells in order, where a probe would decode a batch to read one row.
    pub fn for_program(
        insns: &[(Insn, usize)],
        cursor_ref: &[(Option<CursorKey>, CursorType)],
    ) -> Option<Self> {
        #[derive(Clone, Copy, PartialEq)]
        enum Use {
            Unseen,
            Scanned,
            Sought,
        }
        let mut uses = Vec::new();
        uses.try_reserve_exact(cursor_ref.len()).ok()?;
        uses.resize(cursor_ref.len(), Use::Unseen);
        let mut columns: Vec<Vec<usize>> = Vec::new();
        columns.try_reserve_exact(cursor_ref.len()).ok()?;
        columns.resize_with(cursor_ref.len(), Vec::new);
        for (insn, _) in insns {
            let (cursor, seen) = match insn {
                Insn::Rewind { cursor_id, .. } | Insn::Last { cursor_id, .. } => {
                    (*cursor_id, Use::Scanned)
                }
                Insn::SeekRowid { cursor_id, .. }
                | Insn::SeekEnd { cursor_id }
                | Insn::SeekGE { cursor_id, .. }
                | Insn::SeekGT { cursor_id, .. }
                | Insn::SeekLE { cursor_id, .. }
                | Insn::SeekLT { cursor_id, .. }
                | Insn::NotExists {
                    cursor: cursor_id, ..
                }
                | Insn::DeferredSeek {
                    table_cursor_id: cursor_id,
                    ..
                } => (*cursor_id, Use::Sought),
                Insn::Column {
                    cursor_id, column, ..
                } => {
                    if let Some(read) = columns.get_mut(*cursor_id) {
                        read.try_reserve(1).ok()?;
                        read.push(*column);
                    }
                    continue;
                }
                Insn::ColumnRange {
                    cursor_id,
                    start_column,
                    defaults,
                    ..
                } => {
                    if let Some(read) = columns.get_mut(*cursor_id) {
                        read.try_reserve(defaults.len()).ok()?;
                        for offset in 0..defaults.len() {
                            read.push(start_column.checked_add(offset)?);
                        }
                    }
                    continue;
                }
                _ => continue,
            };
            if let Some(used) = uses.get_mut(cursor) {
                if *used != Use::Sought {
                    *used = seen;
                }
            }
        }
        let mut plan = Self::default();
        plan.columns.try_reserve_exact(cursor_ref.len()).ok()?;
        for ((used, mut read), (_, kind)) in uses.into_iter().zip(columns).zip(cursor_ref) {
            let scanned_table = used == Use::Scanned
                && matches!(kind, CursorType::BTreeTable(table) if table.has_rowid);
            if !scanned_table || read.is_empty() {
                plan.columns.push(None);
                continue;
            }
            read.sort_unstable();
            read.dedup();
            plan.columns.push(Some(read.into_boxed_slice()));
            plan.batched_cursors = plan.batched_cursors.checked_add(1)?;
        }
        if plan.batched_cursors == 0 {
            return None;
        }
        plan.fused = fused::loops(insns, &plan)?;
        Some(plan)
    }

    /// The fused scan loop whose Next is at `next_pc`.
    pub(crate) fn fused_loop(&self, next_pc: usize) -> Option<&fused::FusedLoop> {
        let at = self
            .fused
            .binary_search_by_key(&next_pc, fused::FusedLoop::next_pc)
            .ok()?;
        self.fused.get(at)
    }

    fn columns(&self, cursor_id: usize) -> Option<&[usize]> {
        self.columns.get(cursor_id)?.as_deref()
    }
}

/// Why a scan reads row-at-a-time instead of by batches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchRefusal {
    /// The platform's cache size could not be read, so no batch size can be derived on it.
    CacheSizeUnknown { platform: &'static str },
}

/// One value of a batch.
#[derive(Debug)]
enum Slot {
    Value(Value),
    /// The record holds fewer columns: an `ALTER TABLE ... ADD COLUMN` came after it was
    /// written, and the column's default applies.
    PastEnd,
    /// Not decoded into the batch (the record spills to overflow pages): read row-at-a-time.
    Unbatched,
}

/// The decoded window of one leaf under one cursor.
#[derive(Debug, Default)]
struct LeafBatch {
    page_id: usize,
    peers_epoch: u64,
    /// The cells `first..first + rows` of the leaf.
    first: usize,
    rows: usize,
    /// Column-major: the batch's column `slot` holds `values[slot * rows..(slot + 1) * rows]`.
    values: Vec<Slot>,
    /// Whether the batch holds a decoded window (false after a reset).
    live: bool,
}

impl LeafBatch {
    fn covers(&self, leaf: &TableLeafUnderCursor<'_>) -> bool {
        self.live
            && self.page_id == leaf.contents.id()
            && self.peers_epoch == leaf.peers_epoch
            && leaf.cell >= self.first
            && leaf
                .cell
                .checked_sub(self.first)
                .is_some_and(|row| row < self.rows)
    }

    fn slot(&self, column_slot: usize, cell: usize) -> Option<&Slot> {
        let row = cell.checked_sub(self.first)?;
        let at = column_slot.checked_mul(self.rows)?.checked_add(row)?;
        self.values.get(at)
    }
}

/// A program run's batches, one per batched cursor.
#[derive(Debug, Default)]
pub struct LeafBatches {
    by_cursor: Vec<LeafBatch>,
}

impl LeafBatches {
    /// Forgets every window (a new run, a reset): the memory stays for the next run.
    pub fn clear(&mut self) {
        self.by_cursor.iter_mut().for_each(|batch| {
            batch.live = false;
            batch.values.clear();
        });
    }
}

/// What a batch did for one `Column`.
pub enum Served {
    /// The value is in the register.
    Value,
    /// The record holds fewer columns: the column's default applies.
    PastEnd,
    /// Not from a batch: read the row as before.
    RowAtATime,
}

/// Serves column `column` of the row under `cursor` (cursor id `cursor_id`) from its leaf's
/// batch, decoding a new window when the cursor has moved past the last one.
pub(crate) fn serve_column(
    batches: &mut LeafBatches,
    plan: &BatchPlan,
    cursor_id: usize,
    cursor: &BTreeCursor,
    column: usize,
    dest: &mut Register,
) -> Result<Served> {
    let Some(columns) = plan.columns(cursor_id) else {
        return Ok(Served::RowAtATime);
    };
    let Ok(column_slot) = columns.binary_search(&column) else {
        return Ok(Served::RowAtATime);
    };
    let Some(cell) = window(batches, plan, cursor_id, cursor)? else {
        return Ok(Served::RowAtATime);
    };
    let batch = batches
        .by_cursor
        .get(cursor_id)
        .ok_or_else(|| LimboError::InternalError(format!("batch {cursor_id} out of range")))?;
    match batch.slot(column_slot, cell) {
        Some(Slot::Value(value)) => {
            dest.set_value(value.try_clone().map_err(alloc_error)?);
            Ok(Served::Value)
        }
        Some(Slot::PastEnd) => Ok(Served::PastEnd),
        Some(Slot::Unbatched) | None => Ok(Served::RowAtATime),
    }
}

/// The cell under `cursor`, its leaf's batch decoded around it (a new window when the cursor
/// has moved past the last one), or None when the row is read row-at-a-time.
fn window(
    batches: &mut LeafBatches,
    plan: &BatchPlan,
    cursor_id: usize,
    cursor: &BTreeCursor,
) -> Result<Option<usize>> {
    let Some(columns) = plan.columns(cursor_id) else {
        return Ok(None);
    };
    let Some(leaf) = cursor.table_leaf_under_cursor() else {
        return Ok(None);
    };
    if batches.by_cursor.len() <= cursor_id {
        let slots = cursor_id
            .checked_add(1)
            .ok_or_else(|| LimboError::InternalError("cursor id overflows".into()))?;
        batches
            .by_cursor
            .try_reserve_exact(slots.saturating_sub(batches.by_cursor.len()))
            .map_err(alloc_error)?;
        batches.by_cursor.resize_with(slots, LeafBatch::default);
    }
    let batch = batches
        .by_cursor
        .get_mut(cursor_id)
        .ok_or_else(|| LimboError::InternalError(format!("batch {cursor_id} out of range")))?;
    if !batch.covers(&leaf) {
        let Ok(budget) = budget_bytes(cursor, plan.batched_cursors) else {
            return Ok(None);
        };
        let backward = batch.live && batch.page_id == leaf.contents.id() && leaf.cell < batch.first;
        let rows = decode_window(batch, &leaf, columns, budget, backward)?;
        #[cfg(test)]
        tests::note_decoded(rows);
        if rows == 0 {
            return Ok(None);
        }
    }
    Ok(Some(leaf.cell))
}

/// Decodes the window of the leaf that starts at the cursor's cell (ends at it, `backward`),
/// as many rows as `budget` bytes hold. Returns how many rows it decoded: none when the cursor's
/// own row alone is over the budget (the stated fallback: row-at-a-time).
fn decode_window(
    batch: &mut LeafBatch,
    leaf: &TableLeafUnderCursor<'_>,
    columns: &[usize],
    budget: usize,
    backward: bool,
) -> Result<usize> {
    batch.live = false;
    batch.values.clear();
    let cells = leaf.contents.cell_count();
    let slot_bytes = columns
        .len()
        .checked_mul(std::mem::size_of::<Slot>())
        .ok_or_else(|| LimboError::InternalError("batch slots overflow".into()))?;
    // Pass 1: the window, each row charged its local record bytes and its value slots. Ends at
    // the leaf's edge or the budget: at most the leaf's cell count steps.
    let mut charged = 0usize;
    let mut taken = 0usize;
    loop {
        let next = if backward {
            leaf.cell.checked_sub(taken)
        } else {
            leaf.cell.checked_add(taken)
        };
        let Some(cell) = next.filter(|cell| *cell < cells) else {
            break;
        };
        let record = leaf
            .contents
            .decode_leaf_cell_without_overflow(cell, leaf.limits)
            .map_or(0, |(payload, _)| payload.len());
        let charge = record.saturating_add(slot_bytes);
        match charged.checked_add(charge).filter(|total| *total <= budget) {
            Some(total) => charged = total,
            None => break,
        }
        taken = taken.saturating_add(1);
    }
    let (lo, hi) = if backward {
        let hi = leaf.cell.saturating_add(1);
        (hi.saturating_sub(taken), hi)
    } else {
        (leaf.cell, leaf.cell.saturating_add(taken))
    };
    let rows = hi.saturating_sub(lo);
    if rows == 0 {
        return Ok(0);
    }
    let total = rows
        .checked_mul(columns.len())
        .ok_or_else(|| LimboError::InternalError("batch size overflows".into()))?;
    batch.values.try_reserve_exact(total).map_err(alloc_error)?;
    batch.values.resize_with(total, || Slot::Unbatched);
    // Pass 2: each row's header parsed once, its read columns decoded into their vectors.
    let bound = columns.last().map_or(0, |last| last.saturating_add(1));
    let mut header = RowHeader::new(bound);
    let mut scratch = Register::Value(Value::Null);
    for row in 0..rows {
        let cell = lo.saturating_add(row);
        let Some((payload, _)) = leaf
            .contents
            .decode_leaf_cell_without_overflow(cell, leaf.limits)
        else {
            continue;
        };
        header.forget();
        for (column_slot, column) in columns.iter().enumerate() {
            let value = if header.column_into_register(payload, *column, &mut scratch)? {
                match std::mem::replace(&mut scratch, Register::Value(Value::Null)) {
                    Register::Value(value) => Slot::Value(value),
                    _ => {
                        return Err(LimboError::InternalError(
                            "a decoded column is not a value".into(),
                        ))
                    }
                }
            } else {
                Slot::PastEnd
            };
            let at = column_slot
                .checked_mul(rows)
                .and_then(|start| start.checked_add(row))
                .ok_or_else(|| LimboError::InternalError("batch slot overflows".into()))?;
            if let Some(slot) = batch.values.get_mut(at) {
                *slot = value;
            }
        }
    }
    batch.page_id = leaf.contents.id();
    batch.peers_epoch = leaf.peers_epoch;
    batch.first = lo;
    batch.rows = rows;
    batch.live = true;
    Ok(rows)
}

fn alloc_error<E: std::fmt::Display>(error: E) -> LimboError {
    LimboError::InternalError(format!("a column batch could not be allocated: {error}"))
}

/// The bytes one cursor's batch may hold: the measured second-level cache, or the statement's
/// memory budget shared among its batched cursors when that is smaller.
fn budget_bytes(
    cursor: &BTreeCursor,
    batched_cursors: usize,
) -> std::result::Result<usize, BatchRefusal> {
    #[cfg(test)]
    if let Some(bytes) = tests::budget_override() {
        return Ok(bytes);
    }
    let cache = cache::second_level_bytes()?;
    let share = cursor
        .pager
        .statement_memory_budget()
        .checked_div(batched_cursors)
        .unwrap_or(0);
    Ok(cache.min(share))
}

/// The core's second-level data cache, read from the operating system once per process.
mod cache {
    use super::BatchRefusal;
    use std::sync::OnceLock;

    static SECOND_LEVEL: OnceLock<Result<usize, BatchRefusal>> = OnceLock::new();

    pub(super) fn second_level_bytes() -> Result<usize, BatchRefusal> {
        *SECOND_LEVEL.get_or_init(read)
    }

    /// Linux: the kernel's description of CPU 0's caches (`Documentation/ABI/testing/
    /// sysfs-devices-system-cpu`, `cache/index*/{level,type,size}`).
    #[cfg(target_os = "linux")]
    fn read() -> Result<usize, BatchRefusal> {
        let refused = BatchRefusal::CacheSizeUnknown { platform: "linux" };
        let entries =
            std::fs::read_dir("/sys/devices/system/cpu/cpu0/cache").map_err(|_| refused)?;
        for entry in entries.flatten() {
            let path = entry.path();
            let field = |name: &str| std::fs::read_to_string(path.join(name)).ok();
            let level = field("level");
            let kind = field("type");
            if level.as_deref().map(str::trim) != Some("2")
                || !matches!(kind.as_deref().map(str::trim), Some("Data" | "Unified"))
            {
                continue;
            }
            if let Some(bytes) = field("size").as_deref().and_then(parse_size) {
                return Ok(bytes);
            }
        }
        Err(refused)
    }

    /// macOS: the kernel's `hw.perflevel0.l2cachesize` (the performance cores', Apple silicon)
    /// or `hw.l2cachesize`, through sysctl(8), which needs no unsafe code here.
    #[cfg(target_os = "macos")]
    fn read() -> Result<usize, BatchRefusal> {
        for name in ["hw.perflevel0.l2cachesize", "hw.l2cachesize"] {
            let Ok(out) = std::process::Command::new("/usr/sbin/sysctl")
                .args(["-n", name])
                .output()
            else {
                continue;
            };
            if let Some(bytes) = std::str::from_utf8(&out.stdout)
                .ok()
                .and_then(|text| text.trim().parse::<usize>().ok())
                .filter(|bytes| *bytes > 0)
            {
                return Ok(bytes);
            }
        }
        Err(BatchRefusal::CacheSizeUnknown { platform: "macos" })
    }

    /// Elsewhere (Windows among them) the cache size needs an OS call this crate makes only
    /// through unsafe code (CLAUDE.md §12): scans there read row-at-a-time.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn read() -> Result<usize, BatchRefusal> {
        Err(BatchRefusal::CacheSizeUnknown {
            platform: std::env::consts::OS,
        })
    }

    /// `2048K`, `1M` or a plain byte count, as sysfs writes a cache's size.
    #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
    pub(super) fn parse_size(text: &str) -> Option<usize> {
        let text = text.trim();
        let (digits, unit) = match text.strip_suffix(['K', 'k']) {
            Some(digits) => (digits, 1024usize),
            None => match text.strip_suffix(['M', 'm']) {
                Some(digits) => (digits, 1024usize.checked_mul(1024)?),
                None => (text, 1),
            },
        };
        digits
            .parse::<usize>()
            .ok()?
            .checked_mul(unit)
            .filter(|bytes| *bytes > 0)
    }
}

pub(crate) mod fused;

#[cfg(test)]
mod tests;
