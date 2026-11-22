//! Amphora (query speed step 7, QT.7): a scan loop's body run over a whole batch.
//!
//! A scan loop (`Rewind c; body; Next c` back to the body) whose body only reads the scanned
//! cursor's columns, filters them (comparisons, ranges, IN lists, IS NULL, AND and OR, which the
//! code generator lowers to comparisons and forward jumps) and steps COUNT, SUM, TOTAL, AVG, MIN
//! or MAX is fused: when its Next lands on a row of a decoded batch, the body runs over the rest
//! of the batch at once instead of once a row. Each body instruction gets the selection of rows
//! that reach it, a bit per row: a filter splits its selection between its jump target and the
//! instruction after it, so AND, OR and IN need nothing of their own; an aggregate step folds
//! its whole selection into its accumulator. Rows reach every instruction in ascending order,
//! so order-sensitive aggregates (a float sum, the first of equal minima) fold as row-at-a-time.
//!
//! SQL's semantics stay in one place (CLAUDE.md §13): each comparison is the comparison
//! opcodes' own `comparison_jumps`, a REAL column's affinity is `apply_real_affinity`, and each
//! aggregate step is the opcode's `agg_step_fast` with its slow path `op_agg_step_slow` for
//! what that doesn't take (a first row, text, an overflow: SUM's exact integer overflow error).
//! A row the batch left out (its record spills to overflow pages) ends the fused run before it
//! and is read row-at-a-time.
//!
//! A hash GROUP BY's step (`GroupFind`, its aggregate steps, `GroupSave`) runs as one step over
//! its selection, row by row in order, through the opcodes' own `op_group_find` and
//! `op_group_save`. Between consecutive rows of one group (keys identical value for value, so
//! equal under any collation) the save and the next find are skipped: the group's accumulators
//! stay in their registers. A row the full group table refuses ends the run before it: the
//! cursor stops on the row before, and that row runs row-at-a-time on to the sorter's insert,
//! which no batch runs. So in such a loop no step with an effect stands outside the group step.

use std::borrow::Cow;

use super::{BatchPlan, LeafBatches, Slot};
use crate::error::LimboError;
use crate::function::{AccumulatorFunc, AggFunc};
use crate::numeric::Numeric;
use crate::storage::pager::Pager;
use crate::sync::Arc;
use crate::translate::collate::CollationSeq;
use crate::types::AsValueRef;
use crate::vdbe::execute::{
    agg_step_fast, apply_real_affinity, comparison_jumps, op_agg_step_slow, op_group_find,
    op_group_save, InsnFunctionStepResult, InsnResult,
};
use crate::vdbe::insn::{CmpInsFlags, GroupFindData, Insn};
use crate::vdbe::value::ComparisonOp;
use crate::vdbe::{BranchOffset, Program, ProgramState, Register};
use crate::{Result, Value, ValueRef};

/// A fused scan loop, found once when the program is built.
#[derive(Debug, Clone)]
pub struct FusedLoop {
    cursor_id: usize,
    /// The body's first instruction (the Next's target).
    head: usize,
    /// The Next's address.
    next_pc: usize,
    /// The columns the body reads, each into its own register.
    vectors: Box<[Vector]>,
    /// One step per body instruction, `head..next_pc`.
    steps: Box<[Step]>,
    /// The hash GROUP BY's step, when the body has one.
    group: Option<GroupStep>,
}

impl FusedLoop {
    pub(crate) fn next_pc(&self) -> usize {
        self.next_pc
    }
}

/// A hash GROUP BY's step in a fused loop: `GroupFind` to `GroupSave`.
#[derive(Debug, Clone)]
struct GroupStep {
    find_pc: usize,
    save_pc: usize,
    /// The first of GroupFind's value registers, and each one's operand, the keys first.
    values_start: usize,
    values: Box<[Operand]>,
    keys: usize,
    /// The block's aggregate steps in order: each one's address and argument.
    aggs: Box<[(usize, Operand)]>,
}

/// Where a body's group step lies (`emit_hash_group_step`): its GroupFind, the GroupSave that
/// closes it, then a Goto past the sorter's insert, which only a row the table refused reaches.
#[derive(Debug, Clone, Copy)]
struct GroupBounds {
    find: usize,
    save: usize,
    /// GroupFind's jump for a refused row: the sorter's insert, `full..after`.
    full: usize,
    /// The Goto's target, past the sorter's insert.
    after: usize,
}

impl GroupBounds {
    fn at(insns: &[(Insn, usize)], find: usize, data: &GroupFindData) -> Option<Self> {
        let full = offset(&data.target_pc_full)?;
        let save = full.checked_sub(2)?;
        match insns.get(save) {
            Some((
                Insn::GroupSave {
                    table_id,
                    acc_start,
                    num_accs,
                },
                _,
            )) if *table_id == data.table_id
                && *acc_start == data.acc_start
                && *num_accs == data.num_accs => {}
            _ => return None,
        }
        let Some((Insn::Goto { target_pc }, _)) = insns.get(save.checked_add(1)?) else {
            return None;
        };
        let after = offset(target_pc)?;
        (find < save && after > full).then_some(Self {
            find,
            save,
            full,
            after,
        })
    }

    /// Between GroupFind and GroupSave: the aggregate steps and their arguments' columns.
    fn inside(&self, pc: usize) -> bool {
        pc > self.find && pc < self.save
    }

    /// The sorter's insert, which only a refused row reaches.
    fn refused_only(&self, pc: usize) -> bool {
        pc >= self.full && pc < self.after
    }

    /// Whether a filter's jump may not land on `target`: in the block, on the Goto after it, or
    /// in the sorter's insert.
    fn closed_to(&self, target: usize) -> bool {
        (target > self.find && target <= self.save + 1) || self.refused_only(target)
    }
}

#[derive(Debug, Clone)]
struct Vector {
    /// The register the body's Column writes.
    reg: usize,
    /// The Column's address.
    pc: usize,
    column: usize,
    /// The value a record that ends before the column reads (`ALTER TABLE ... ADD COLUMN`).
    default: Value,
    /// A RealAffinity follows the Column.
    real: bool,
}

#[derive(Debug, Clone, Copy)]
enum Operand {
    /// A column vector, by index into `vectors`.
    Vector(usize),
    /// A register the body doesn't write (a constant, a parameter).
    Register(usize),
}

#[derive(Debug, Clone, Copy)]
enum Step {
    /// Column or RealAffinity: the vector is already there.
    Fall,
    Goto(usize),
    Compare {
        lhs: Operand,
        rhs: Operand,
        target: usize,
        flags: CmpInsFlags,
        collation: Option<CollationSeq>,
        op: ComparisonOp,
    },
    Null {
        operand: Operand,
        target: usize,
        jump_if_null: bool,
    },
    /// The AggStep at this address, its argument read from `arg`.
    Agg {
        pc: usize,
        arg: Operand,
    },
    /// The GroupFind that opens the loop's group step.
    Group,
    /// Within the group step, which runs it: nothing of its own.
    Inner,
    /// The sorter's insert after a group step, which a batch never runs.
    Refused,
}

/// The program's fused loops, ascending by their Next's address. None only when memory for
/// them can't be had.
pub(super) fn loops(insns: &[(Insn, usize)], plan: &BatchPlan) -> Option<Vec<FusedLoop>> {
    let mut out = Vec::new();
    for (pc, (insn, _)) in insns.iter().enumerate() {
        let Insn::Rewind { cursor_id, .. } = insn else {
            continue;
        };
        if plan.columns(*cursor_id).is_none() {
            continue;
        }
        if let Some(fused) = loop_at(insns, *cursor_id, pc.checked_add(1)?) {
            out.try_reserve(1).ok()?;
            out.push(fused);
        }
    }
    out.sort_unstable_by_key(|fused| fused.next_pc);
    Some(out)
}

fn offset(target: &BranchOffset) -> Option<usize> {
    match target {
        BranchOffset::Offset(at) => usize::try_from(*at).ok(),
        _ => None,
    }
}

/// A comparison opcode's operands, jump target, flags, collation and operator.
type Comparison<'a> = (
    usize,
    usize,
    &'a BranchOffset,
    CmpInsFlags,
    Option<CollationSeq>,
    ComparisonOp,
);

fn comparison(insn: &Insn) -> Option<Comparison<'_>> {
    Some(match insn {
        Insn::Eq {
            lhs,
            rhs,
            target_pc,
            flags,
            collation,
        } => (*lhs, *rhs, target_pc, *flags, *collation, ComparisonOp::Eq),
        Insn::Ne {
            lhs,
            rhs,
            target_pc,
            flags,
            collation,
        } => (*lhs, *rhs, target_pc, *flags, *collation, ComparisonOp::Ne),
        Insn::Lt {
            lhs,
            rhs,
            target_pc,
            flags,
            collation,
        } => (*lhs, *rhs, target_pc, *flags, *collation, ComparisonOp::Lt),
        Insn::Le {
            lhs,
            rhs,
            target_pc,
            flags,
            collation,
        } => (*lhs, *rhs, target_pc, *flags, *collation, ComparisonOp::Le),
        Insn::Gt {
            lhs,
            rhs,
            target_pc,
            flags,
            collation,
        } => (*lhs, *rhs, target_pc, *flags, *collation, ComparisonOp::Gt),
        Insn::Ge {
            lhs,
            rhs,
            target_pc,
            flags,
            collation,
        } => (*lhs, *rhs, target_pc, *flags, *collation, ComparisonOp::Ge),
        _ => return None,
    })
}

/// The loop whose body starts at `head`, when its body is one this module runs.
fn loop_at(insns: &[(Insn, usize)], cursor_id: usize, head: usize) -> Option<FusedLoop> {
    // Pass 1: the Next that closes the loop, the registers the body writes, and its group step,
    // whose sorter's insert (reached only by a refused row) no batch runs.
    let mut written: Vec<usize> = Vec::new();
    let mut next_pc = None;
    let mut group: Option<GroupBounds> = None;
    for (pc, (insn, _)) in insns.iter().enumerate().skip(head) {
        if group.is_some_and(|group| group.refused_only(pc)) {
            continue;
        }
        let inside = group.is_some_and(|group| group.inside(pc));
        match insn {
            Insn::Next {
                cursor_id: c,
                pc_if_next,
                ..
            } if *c == cursor_id => {
                if offset(pc_if_next)? != head {
                    return None;
                }
                next_pc = Some(pc);
                break;
            }
            Insn::Column {
                cursor_id: c, dest, ..
            } if *c == cursor_id => {
                written.try_reserve(1).ok()?;
                written.push(*dest);
            }
            Insn::ColumnRange {
                cursor_id: c,
                dest,
                defaults,
                ..
            } if *c == cursor_id => {
                written.try_reserve(defaults.len()).ok()?;
                for at in 0..defaults.len() {
                    written.push(dest.checked_add(at)?);
                }
            }
            Insn::AggStep { data } => {
                written.try_reserve(1).ok()?;
                written.push(data.acc_reg);
            }
            Insn::RealAffinity { .. } => {}
            Insn::GroupFind { data } if group.is_none() => {
                let bounds = GroupBounds::at(insns, pc, data)?;
                written.try_reserve(data.num_accs).ok()?;
                for at in 0..data.num_accs {
                    written.push(data.acc_start.checked_add(at)?);
                }
                group = Some(bounds);
            }
            Insn::GroupSave { .. } if group.is_some_and(|group| group.save == pc) => {}
            Insn::Goto { .. } | Insn::IsNull { .. } | Insn::NotNull { .. } if !inside => {}
            other if !inside && comparison(other).is_some() => {}
            _ => return None,
        }
    }
    let next_pc = next_pc?;
    if next_pc <= head || group.is_some_and(|group| group.after > next_pc) {
        return None;
    }
    // Pass 2: each instruction's step, its operands resolved.
    let mut vectors: Vec<Vector> = Vec::new();
    let mut steps: Vec<Step> = Vec::new();
    let mut group_step: Option<GroupStep> = None;
    let mut aggs: Vec<(usize, Operand)> = Vec::new();
    steps.try_reserve_exact(next_pc - head).ok()?;
    let resolve = |vectors: &[Vector], reg: usize| -> Option<Operand> {
        if let Some(at) = vectors.iter().position(|vector| vector.reg == reg) {
            return Some(Operand::Vector(at));
        }
        // Written in the body but not (yet) a column: read before its Column, or an
        // accumulator.
        (!written.contains(&reg)).then_some(Operand::Register(reg))
    };
    let forward = |pc: usize, target: &BranchOffset| -> Option<usize> {
        offset(target).filter(|target| {
            *target > pc
                && *target <= next_pc
                && !group.is_some_and(|group| group.closed_to(*target))
        })
    };
    // The Column or ColumnRange a RealAffinity may follow: the last one, with only
    // RealAffinity since.
    let mut columns_at: Option<usize> = None;
    for pc in head..next_pc {
        let (insn, _) = insns.get(pc)?;
        let in_group = group.is_some_and(|group| group.inside(pc) || group.save == pc);
        let mut columns_since = None;
        let step = match insn {
            _ if group.is_some_and(|group| group.refused_only(pc)) => Step::Refused,
            Insn::Column {
                column,
                dest,
                default,
                ..
            } => {
                push_vector(&mut vectors, *dest, pc, *column, default)?;
                columns_since = Some(pc);
                Step::Fall
            }
            Insn::ColumnRange {
                start_column,
                dest,
                defaults,
                ..
            } => {
                for (at, default) in defaults.iter().enumerate() {
                    push_vector(
                        &mut vectors,
                        dest.checked_add(at)?,
                        pc,
                        start_column.checked_add(at)?,
                        default,
                    )?;
                }
                columns_since = Some(pc);
                Step::Fall
            }
            Insn::RealAffinity { register } => {
                let from = columns_at?;
                let vector = vectors
                    .iter_mut()
                    .find(|vector| vector.reg == *register && vector.pc == from)?;
                vector.real = true;
                columns_since = Some(from);
                Step::Fall
            }
            Insn::Goto { target_pc } => Step::Goto(forward(pc, target_pc)?),
            Insn::IsNull { reg, target_pc } | Insn::NotNull { reg, target_pc } => Step::Null {
                operand: resolve(&vectors, *reg)?,
                target: forward(pc, target_pc)?,
                jump_if_null: matches!(insn, Insn::IsNull { .. }),
            },
            Insn::AggStep { data } => {
                let AccumulatorFunc::Agg(
                    AggFunc::Count
                    | AggFunc::Count0
                    | AggFunc::Sum
                    | AggFunc::Total
                    | AggFunc::Avg
                    | AggFunc::Min
                    | AggFunc::Max,
                ) = data.func
                else {
                    return None;
                };
                let arg = resolve(&vectors, data.col)?;
                if in_group {
                    aggs.try_reserve(1).ok()?;
                    aggs.push((pc, arg));
                    Step::Inner
                } else if group.is_some() {
                    // A step with an effect outside the group step would run for rows past
                    // where a refused row ends the run.
                    return None;
                } else {
                    Step::Agg { pc, arg }
                }
            }
            Insn::GroupFind { data } => {
                if group.map(|group| group.find) != Some(pc) || data.num_keys > data.num_values {
                    return None;
                }
                let mut values = Vec::new();
                values.try_reserve_exact(data.num_values).ok()?;
                for at in 0..data.num_values {
                    values.push(resolve(&vectors, data.values_start.checked_add(at)?)?);
                }
                group_step = Some(GroupStep {
                    find_pc: pc,
                    save_pc: group.map(|group| group.save)?,
                    values_start: data.values_start,
                    values: values.into_boxed_slice(),
                    keys: data.num_keys,
                    aggs: Box::default(),
                });
                Step::Group
            }
            Insn::GroupSave { .. } => Step::Inner,
            other => {
                let (lhs, rhs, target, flags, collation, op) = comparison(other)?;
                Step::Compare {
                    lhs: resolve(&vectors, lhs)?,
                    rhs: resolve(&vectors, rhs)?,
                    target: forward(pc, target)?,
                    flags,
                    collation,
                    op,
                }
            }
        };
        columns_at = columns_since;
        steps.push(if in_group && matches!(step, Step::Fall) {
            Step::Inner
        } else {
            step
        });
    }
    if let Some(step) = group_step.as_mut() {
        step.aggs = aggs.into_boxed_slice();
    }
    if group.is_some() != group_step.is_some() {
        return None;
    }
    Some(FusedLoop {
        cursor_id,
        head,
        next_pc,
        vectors: vectors.into_boxed_slice(),
        steps: steps.into_boxed_slice(),
        group: group_step,
    })
}

/// A column vector the body's Column (or one column of its ColumnRange) writes into `reg`.
fn push_vector(
    vectors: &mut Vec<Vector>,
    reg: usize,
    pc: usize,
    column: usize,
    default: &Option<Value>,
) -> Option<()> {
    if vectors.iter().any(|vector| vector.reg == reg) {
        return None;
    }
    vectors.try_reserve(1).ok()?;
    vectors.push(Vector {
        reg,
        pc,
        column,
        default: default.clone().unwrap_or(Value::Null),
        real: false,
    });
    Some(())
}

/// Runs `fused`'s body over the batch from the row under its cursor: as many rows as the batch
/// holds from there, up to a row it left out or a row its full group table refused. Leaves the
/// cursor on the last row it ran and returns how many it ran: none when the row under the
/// cursor isn't in a batch or is refused (it runs row-at-a-time).
pub(crate) fn run(
    program: &Program,
    state: &mut ProgramState,
    fused: &FusedLoop,
    pager: &Arc<Pager>,
) -> Result<usize> {
    let mut batches = std::mem::take(&mut state.leaf_batches);
    let ran = run_batch(program, state, &mut batches, fused, pager);
    state.leaf_batches = batches;
    ran
}

fn internal(what: &str) -> LimboError {
    LimboError::InternalError(format!("fused scan: {what}"))
}

fn run_batch(
    program: &Program,
    state: &mut ProgramState,
    batches: &mut LeafBatches,
    fused: &FusedLoop,
    pager: &Arc<Pager>,
) -> Result<usize> {
    let Some(plan) = program.leaf_batch_plan.as_deref() else {
        return Ok(0);
    };
    let Some(columns) = plan.columns(fused.cursor_id) else {
        return Ok(0);
    };
    let cursor = match state.cursors.get_mut(fused.cursor_id) {
        Some(Some(crate::types::Cursor::BTree(cursor, ..))) => cursor,
        _ => return Ok(0),
    };
    let Some(cell) = super::window(batches, plan, fused.cursor_id, cursor)? else {
        return Ok(0);
    };
    let batch = batches
        .by_cursor
        .get(fused.cursor_id)
        .ok_or_else(|| internal("no batch"))?;
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(fused.vectors.len())
        .map_err(super::alloc_error)?;
    for vector in fused.vectors.iter() {
        slots.push(
            columns
                .binary_search(&vector.column)
                .map_err(|_| internal("a column not in the batch"))?,
        );
    }
    // The rows from the cursor's to the batch's end or the first row it left out.
    let end = batch.first.saturating_add(batch.rows);
    let mut rows = 0usize;
    'rows: for at in cell..end {
        for slot in &slots {
            if !matches!(batch.slot(*slot, at), Some(Slot::Value(_) | Slot::PastEnd)) {
                break 'rows;
            }
        }
        rows += 1;
    }
    // The cursor moves once the body has run, to the last row it ran: a refused row ends the
    // run short of the batch's end.
    if rows == 0 || !cursor.can_advance_within_leaf_to(cell + rows - 1) {
        return Ok(0);
    }
    let reader = Reader {
        fused,
        slots: &slots,
        batch,
        cell,
    };
    // The selection of each body instruction, `words` bits a row, and one for the Next.
    let words = rows.div_ceil(64);
    let stride = fused
        .steps
        .len()
        .checked_add(1)
        .ok_or_else(|| internal("too many steps"))?;
    let reach = &mut batches_scratch(
        stride
            .checked_mul(words)
            .ok_or_else(|| internal("selection overflows"))?,
    )?;
    for row in 0..rows {
        set(reach, 0, words, row);
    }
    let mut selected: Vec<usize> = Vec::new();
    selected
        .try_reserve_exact(rows)
        .map_err(super::alloc_error)?;
    // The rows the run covers: all of them, or those before the row a full group table refused.
    let mut ran = rows;
    for (index, step) in fused.steps.iter().enumerate() {
        selected.clear();
        for word in 0..words {
            let mut bits = reach.get(index * words + word).copied().unwrap_or(0);
            while bits != 0 {
                selected.push(word * 64 + bits.trailing_zeros() as usize);
                bits &= bits - 1;
            }
        }
        if selected.is_empty() {
            continue;
        }
        let next = index + 1;
        match *step {
            Step::Fall => selected
                .iter()
                .for_each(|row| set(reach, next, words, *row)),
            Step::Inner => {}
            Step::Refused => return Err(internal("a row reached the sorter's insert")),
            Step::Goto(target) => {
                let to = target - fused.head;
                selected.iter().for_each(|row| set(reach, to, words, *row));
            }
            Step::Compare {
                lhs,
                rhs,
                target,
                flags,
                collation,
                op,
            } => {
                let to = target - fused.head;
                for row in &selected {
                    let jumps = comparison_jumps(
                        program,
                        &*reader.value(lhs, *row, &state.registers)?,
                        &*reader.value(rhs, *row, &state.registers)?,
                        flags,
                        collation,
                        op,
                    )?;
                    set(reach, if jumps { to } else { next }, words, *row);
                }
            }
            Step::Null {
                operand,
                target,
                jump_if_null,
            } => {
                let to = target - fused.head;
                for row in &selected {
                    let null =
                        matches!(*reader.value(operand, *row, &state.registers)?, Value::Null);
                    set(
                        reach,
                        if null == jump_if_null { to } else { next },
                        words,
                        *row,
                    );
                }
            }
            Step::Agg { pc, arg } => {
                for row in &selected {
                    agg_row(program, state, &reader, pc, arg, *row)?;
                }
                selected
                    .iter()
                    .for_each(|row| set(reach, next, words, *row));
            }
            Step::Group => {
                let group = fused
                    .group
                    .as_ref()
                    .ok_or_else(|| internal("no group step"))?;
                let (stepped, refused) =
                    group_rows(program, state, pager, &reader, group, &selected)?;
                // The rows it stepped went through the block and on to the Goto after it.
                let first = group.find_pc + 1 - fused.head;
                let goto = group.save_pc + 1 - fused.head;
                for row in selected.iter().take(stepped) {
                    for index in first..=goto {
                        set(reach, index, words, *row);
                    }
                }
                #[cfg(test)]
                super::tests::note_cut(refused.is_some_and(|row| row > 0));
                if let Some(row) = refused {
                    ran = row;
                }
            }
        }
    }
    if ran == 0 {
        return Ok(0);
    }
    let moved = match state.cursors.get_mut(fused.cursor_id) {
        Some(Some(crate::types::Cursor::BTree(cursor, ..))) => {
            cursor.advance_within_leaf_to(cell + ran - 1)
        }
        _ => false,
    };
    if !moved {
        return Err(internal("the cursor can't move to the run's last row"));
    }
    #[cfg(test)]
    super::tests::note_fused(ran);
    // The last row leaves its columns in their registers, as row-at-a-time does.
    let last = ran - 1;
    for (at, vector) in fused.vectors.iter().enumerate() {
        let index = vector.pc - fused.head;
        if get(reach, index, words, last) {
            let read = reader
                .value(Operand::Vector(at), last, &state.registers)?
                .into_owned();
            if let Some(reg) = state.registers.get_mut(vector.reg) {
                *reg = Register::Value(read);
            }
        }
    }
    Ok(ran)
}

/// One row's step of the AggStep at `pc`: the fast path, else the opcode's own slow path.
fn agg_row(
    program: &Program,
    state: &mut ProgramState,
    reader: &Reader<'_>,
    pc: usize,
    arg: Operand,
    row: usize,
) -> Result<()> {
    let Some((Insn::AggStep { data }, _)) = program.insns.get(pc) else {
        return Err(internal("not an aggregate step"));
    };
    let number = match arg {
        Operand::Register(reg) => match state.registers.get(reg) {
            Some(Register::Value(Value::Numeric(number))) => Some(*number),
            _ => None,
        },
        Operand::Vector(_) => match &*reader.value(arg, row, &state.registers)? {
            Value::Numeric(number) => Some(*number),
            _ => None,
        },
    };
    let acc = state
        .registers
        .get_mut(data.acc_reg)
        .ok_or_else(|| internal("accumulator out of range"))?;
    if agg_step_fast(data, number, acc) {
        return Ok(());
    }
    if let Operand::Vector(_) = arg {
        let read = reader.value(arg, row, &state.registers)?.into_owned();
        let reg = state
            .registers
            .get_mut(data.col)
            .ok_or_else(|| internal("argument out of range"))?;
        *reg = Register::Value(read);
    }
    let pc = state.pc;
    let stepped = op_agg_step_slow(program, state, data)?;
    state.pc = pc;
    if !matches!(stepped, InsnFunctionStepResult::Step) {
        return Err(internal("an aggregate step waited on I/O"));
    }
    Ok(())
}

/// The group step over `selected`, in order: each row's group found (or made) by the opcode's
/// own GroupFind, its aggregates stepped, and its group saved back by GroupSave before a row of
/// another group and at the end; a row of the group just stepped skips the save and the find.
/// Returns how many of `selected` it stepped, and the row a full table refused, which the run
/// ends before.
fn group_rows(
    program: &Program,
    state: &mut ProgramState,
    pager: &Arc<Pager>,
    reader: &Reader<'_>,
    group: &GroupStep,
    selected: &[usize],
) -> Result<(usize, Option<usize>)> {
    let (Some((find, _)), Some((save, _))) = (
        program.insns.get(group.find_pc),
        program.insns.get(group.save_pc),
    ) else {
        return Err(internal("no group step"));
    };
    let found = u32::try_from(group.find_pc + 1).map_err(|_| internal("address out of range"))?;
    let pc = state.pc;
    let mut loaded: Option<usize> = None;
    let mut refused = None;
    let mut stepped = 0usize;
    for row in selected {
        let same = match loaded {
            Some(prev) => same_keys(reader, group, prev, *row, &state.registers)?,
            None => false,
        };
        #[cfg(test)]
        super::tests::note_same_group(same);
        if !same {
            if loaded.take().is_some() {
                group_op(op_group_save, program, state, pager, save, group.save_pc)?;
            }
            for (at, operand) in group.values.iter().enumerate() {
                if let Operand::Vector(_) = operand {
                    let read = reader.value(*operand, *row, &state.registers)?.into_owned();
                    let reg = state
                        .registers
                        .get_mut(group.values_start + at)
                        .ok_or_else(|| internal("a group value out of range"))?;
                    *reg = Register::Value(read);
                }
            }
            group_op(op_group_find, program, state, pager, find, group.find_pc)?;
            if state.pc != found {
                refused = Some(*row);
                break;
            }
            loaded = Some(*row);
        }
        for (agg_pc, arg) in group.aggs.iter() {
            agg_row(program, state, reader, *agg_pc, *arg, *row)?;
        }
        stepped += 1;
    }
    if loaded.is_some() {
        group_op(op_group_save, program, state, pager, save, group.save_pc)?;
    }
    state.pc = pc;
    Ok((stepped, refused))
}

/// Runs a group opcode as the instruction at `at`.
fn group_op(
    op: fn(&Program, &mut ProgramState, &Insn, &Arc<Pager>) -> InsnResult,
    program: &Program,
    state: &mut ProgramState,
    pager: &Arc<Pager>,
    insn: &Insn,
    at: usize,
) -> Result<()> {
    state.pc = u32::try_from(at).map_err(|_| internal("address out of range"))?;
    match op(program, state, insn, pager)? {
        InsnFunctionStepResult::Step => Ok(()),
        _ => Err(internal("a group step waited")),
    }
}

/// Whether rows `prev` and `row` have identical GROUP BY keys.
fn same_keys(
    reader: &Reader<'_>,
    group: &GroupStep,
    prev: usize,
    row: usize,
    registers: &[Register],
) -> Result<bool> {
    for operand in group.values.iter().take(group.keys) {
        let (a, b) = (
            reader.value(*operand, prev, registers)?,
            reader.value(*operand, row, registers)?,
        );
        if !identical(&a, &b) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether two values are the same value of the same storage class, bit for bit: then they
/// are one group under any collation a GROUP BY compares with, each being reflexive. Values
/// equal only under a collation, or across classes, take the find.
fn identical(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Numeric(Numeric::Integer(x)), Value::Numeric(Numeric::Integer(y))) => x == y,
        (Value::Numeric(Numeric::Float(x)), Value::Numeric(Numeric::Float(y))) => {
            f64::from(*x).to_bits() == f64::from(*y).to_bits()
        }
        (Value::Text(x), Value::Text(y)) => x.as_str() == y.as_str(),
        _ => matches!(
            (a.as_value_ref(), b.as_value_ref()),
            (ValueRef::Blob(x), ValueRef::Blob(y)) if x == y
        ),
    }
}

/// Reads a body's operands for a row of the batch.
struct Reader<'a> {
    fused: &'a FusedLoop,
    slots: &'a [usize],
    batch: &'a super::LeafBatch,
    cell: usize,
}

impl<'a> Reader<'a> {
    fn value<'r>(
        &self,
        operand: Operand,
        row: usize,
        registers: &'r [Register],
    ) -> Result<Cow<'r, Value>>
    where
        'a: 'r,
    {
        match operand {
            Operand::Register(reg) => Ok(Cow::Borrowed(
                registers
                    .get(reg)
                    .ok_or_else(|| internal("register out of range"))?
                    .get_value(),
            )),
            Operand::Vector(at) => {
                let vector: &'a Vector = self
                    .fused
                    .vectors
                    .get(at)
                    .ok_or_else(|| internal("no vector"))?;
                let slot = self.slots.get(at).ok_or_else(|| internal("no slot"))?;
                let read: &'a Value = match self.batch.slot(*slot, self.cell + row) {
                    Some(Slot::Value(value)) => value,
                    Some(Slot::PastEnd) => &vector.default,
                    _ => return Err(internal("a row left out of the batch")),
                };
                if vector.real
                    && matches!(read, Value::Numeric(crate::numeric::Numeric::Integer(_)))
                {
                    let mut register = Register::Value(read.clone());
                    apply_real_affinity(&mut register);
                    return Ok(Cow::Owned(register.get_value().clone()));
                }
                Ok(Cow::Borrowed(read))
            }
        }
    }
}

fn batches_scratch(len: usize) -> Result<Vec<u64>> {
    let mut bits = Vec::new();
    bits.try_reserve_exact(len).map_err(super::alloc_error)?;
    bits.resize(len, 0);
    Ok(bits)
}

fn set(reach: &mut [u64], index: usize, words: usize, row: usize) {
    if let Some(word) = reach.get_mut(index * words + row / 64) {
        *word |= 1 << (row % 64);
    }
}

fn get(reach: &[u64], index: usize, words: usize, row: usize) -> bool {
    reach
        .get(index * words + row / 64)
        .is_some_and(|word| word & (1 << (row % 64)) != 0)
}
