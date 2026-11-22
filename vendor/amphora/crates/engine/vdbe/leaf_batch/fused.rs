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

use std::borrow::Cow;

use super::{BatchPlan, LeafBatches, Slot};
use crate::error::LimboError;
use crate::function::{AccumulatorFunc, AggFunc};
use crate::translate::collate::CollationSeq;
use crate::vdbe::execute::{
    agg_step_fast, apply_real_affinity, comparison_jumps, op_agg_step_slow, InsnFunctionStepResult,
};
use crate::vdbe::insn::{CmpInsFlags, Insn};
use crate::vdbe::value::ComparisonOp;
use crate::vdbe::{BranchOffset, Program, ProgramState, Register};
use crate::{Result, Value};

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
}

impl FusedLoop {
    pub(crate) fn next_pc(&self) -> usize {
        self.next_pc
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
    // Pass 1: the Next that closes the loop, and the registers the body writes.
    let mut written: Vec<usize> = Vec::new();
    let mut next_pc = None;
    for (pc, (insn, _)) in insns.iter().enumerate().skip(head) {
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
            Insn::AggStep { data } => {
                written.try_reserve(1).ok()?;
                written.push(data.acc_reg);
            }
            Insn::RealAffinity { .. }
            | Insn::Goto { .. }
            | Insn::IsNull { .. }
            | Insn::NotNull { .. } => {}
            other if comparison(other).is_some() => {}
            _ => return None,
        }
    }
    let next_pc = next_pc?;
    if next_pc <= head {
        return None;
    }
    // Pass 2: each instruction's step, its operands resolved.
    let mut vectors: Vec<Vector> = Vec::new();
    let mut steps: Vec<Step> = Vec::new();
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
        offset(target).filter(|target| *target > pc && *target <= next_pc)
    };
    for pc in head..next_pc {
        let (insn, _) = insns.get(pc)?;
        let step = match insn {
            Insn::Column {
                column,
                dest,
                default,
                ..
            } => {
                if vectors.iter().any(|vector| vector.reg == *dest) {
                    return None;
                }
                vectors.try_reserve(1).ok()?;
                vectors.push(Vector {
                    reg: *dest,
                    pc,
                    column: *column,
                    default: default.clone().unwrap_or(Value::Null),
                    real: false,
                });
                Step::Fall
            }
            Insn::RealAffinity { register } => {
                let last = vectors.last_mut()?;
                if last.reg != *register || last.pc.checked_add(1)? != pc {
                    return None;
                }
                last.real = true;
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
                Step::Agg {
                    pc,
                    arg: resolve(&vectors, data.col)?,
                }
            }
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
        steps.push(step);
    }
    Some(FusedLoop {
        cursor_id,
        head,
        next_pc,
        vectors: vectors.into_boxed_slice(),
        steps: steps.into_boxed_slice(),
    })
}

/// Runs `fused`'s body over the batch from the row under its cursor: as many rows as the batch
/// holds from there, up to a row it left out. Leaves the cursor on the last row it ran and
/// returns how many it ran: none when the row under the cursor isn't in a batch (it runs
/// row-at-a-time).
pub(crate) fn run(program: &Program, state: &mut ProgramState, fused: &FusedLoop) -> Result<usize> {
    let mut batches = std::mem::take(&mut state.leaf_batches);
    let ran = run_batch(program, state, &mut batches, fused);
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
    if rows == 0 || !cursor.advance_within_leaf_to(cell + rows - 1) {
        return Ok(0);
    }
    #[cfg(test)]
    super::tests::note_fused(rows);
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
                let Some((Insn::AggStep { data }, _)) = program.insns.get(pc) else {
                    return Err(internal("not an aggregate step"));
                };
                for row in &selected {
                    let number = match arg {
                        Operand::Register(reg) => match state.registers.get(reg) {
                            Some(Register::Value(Value::Numeric(number))) => Some(*number),
                            _ => None,
                        },
                        Operand::Vector(_) => match &*reader.value(arg, *row, &state.registers)? {
                            Value::Numeric(number) => Some(*number),
                            _ => None,
                        },
                    };
                    let acc = state
                        .registers
                        .get_mut(data.acc_reg)
                        .ok_or_else(|| internal("accumulator out of range"))?;
                    if agg_step_fast(data, number, acc) {
                        continue;
                    }
                    if let Operand::Vector(_) = arg {
                        let read = reader.value(arg, *row, &state.registers)?.into_owned();
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
                }
                selected
                    .iter()
                    .for_each(|row| set(reach, next, words, *row));
            }
        }
    }
    // The last row leaves its columns in their registers, as row-at-a-time does.
    let last = rows - 1;
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
    Ok(rows)
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
