use super::*;
use crate::translate::plan::BitSet;
use crate::vdbe::insn::NullMatchingMask;
use amphora_engine_parser::ast::NullsOrder;

fn index_seek_affinities(seek_def: &SeekDef, seek_key: &SeekKey) -> String {
    // Apply the constraint's resolved comparison affinity to the seek key,
    // not the indexed column's affinity.
    seek_def
        .iter(seek_key)
        .zip(seek_def.iter_affinity(seek_key))
        .map(|(key_component, aff)| match key_component {
            SeekKeyComponent::Expr(expr) if aff.expr_needs_no_affinity_change(expr) => {
                affinity::SQLITE_AFF_BLOB
            }
            _ => aff.aff_mask(),
        })
        .collect()
}

fn encode_seek_keys_for_custom_types(
    program: &mut ProgramBuilder,
    tables: &TableReferences,
    seek_index: &Arc<Index>,
    start_reg: usize,
    num_keys: usize,
    idx_col_offset: usize,
    resolver: &Resolver<'_>,
) -> crate::Result<()> {
    let table = tables
        .find_table_by_identifier(&seek_index.table_name)
        .or_else(|| tables.find_table_by_table_name(&seek_index.table_name));
    let table = match table {
        Some(t) => t,
        None => return Ok(()),
    };
    let columns = table.columns();
    for i in 0..num_keys {
        let idx_col_pos = idx_col_offset + i;
        if idx_col_pos >= seek_index.columns.len() {
            break;
        }
        let idx_col = &seek_index.columns[idx_col_pos];
        let table_col = match columns.get(idx_col.pos_in_table) {
            Some(c) => c,
            None => continue,
        };
        let Ok(Some(resolved)) = resolver
            .schema()
            .resolve_type(&table_col.ty_str, table.is_strict())
        else {
            continue;
        };
        if resolved.chain.iter().all(|td| td.encode().is_none()) {
            continue;
        }
        let reg = start_reg + i;
        let skip_label = program.allocate_label();
        program.emit_insn(Insn::IsNull {
            reg,
            target_pc: skip_label,
        });
        for td in &resolved.chain {
            if let Some(encode_expr) = td.encode() {
                crate::translate::expr::emit_type_expr(
                    program,
                    encode_expr,
                    reg,
                    reg,
                    table_col,
                    td,
                    resolver,
                )?;
            }
        }
        program.preassign_label_to_next_insn(skip_label);
    }
    Ok(())
}

/// Seek-based loop setup.
///
/// A seek loop has a real two-phase contract:
/// 1. Emit and position using the start bound.
/// 2. Emit the termination bound and anchor `loop_start`.
pub(super) struct SeekEmitter<'a, 'plan> {
    program: &'a mut ProgramBuilder,
    tables: &'a TableReferences,
    seek_def: &'a SeekDef,
    t_ctx: &'a mut TranslateCtx<'plan>,
    seek_cursor_id: usize,
    start_reg: usize,
    loop_end: BranchOffset,
    seek_index: Option<&'a Arc<Index>>,
    is_index: bool,
    /// Amphora: where a skip-scan seeks the next distinct value of its skipped columns.
    skip_next: Option<BranchOffset>,
}

impl<'a, 'plan> SeekEmitter<'a, 'plan> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        program: &'a mut ProgramBuilder,
        tables: &'a TableReferences,
        seek_def: &'a SeekDef,
        t_ctx: &'a mut TranslateCtx<'plan>,
        seek_cursor_id: usize,
        start_reg: usize,
        loop_end: BranchOffset,
        seek_index: Option<&'a Arc<Index>>,
    ) -> Self {
        Self {
            program,
            tables,
            seek_def,
            t_ctx,
            seek_cursor_id,
            start_reg,
            loop_end,
            seek_index,
            is_index: seek_index.is_some(),
            skip_next: None,
        }
    }

    /// The registers `key` takes: the skipped columns' and the key's own.
    fn key_len(&self, key: &SeekKey) -> Result<usize> {
        self.seek_def
            .skip
            .checked_add(self.seek_def.size(key))
            .ok_or_else(|| crate::LimboError::InternalError("seek key too long".to_string()))
    }

    /// The first register of the key's own components, after the skipped columns'.
    fn key_reg(&self) -> Result<usize> {
        self.start_reg
            .checked_add(self.seek_def.skip)
            .ok_or_else(|| crate::LimboError::InternalError("seek key too long".to_string()))
    }

    /// Affinities for `key`: none for the skipped columns, read from the index as they are.
    fn key_affinities(&self, key: &SeekKey) -> String {
        std::iter::repeat_n(affinity::SQLITE_AFF_BLOB, self.seek_def.skip)
            .chain(index_seek_affinities(self.seek_def, key).chars())
            .collect()
    }

    /// Amphora: a skip-scan's outer loop, as SQLite codes it (`codeAllEqualityTerms`,
    /// `docs/design/planner.md` decision 4): the first entry's skipped columns go to the key's
    /// first registers, the seek within them follows, and when their range ends the next
    /// distinct value is the first entry past them.
    fn emit_skip_scan_start(&mut self) {
        let first = self.program.allocate_label();
        let skip_next = self.program.allocate_label();
        self.program.emit_insn(Insn::Rewind {
            cursor_id: self.seek_cursor_id,
            pc_if_empty: self.loop_end,
        });
        self.program.emit_insn(Insn::Goto { target_pc: first });
        self.program.preassign_label_to_next_insn(skip_next);
        self.program.emit_insn(Insn::SeekGT {
            is_index: true,
            cursor_id: self.seek_cursor_id,
            start_reg: self.start_reg,
            num_regs: self.seek_def.skip,
            target_pc: self.loop_end,
        });
        self.program.preassign_label_to_next_insn(first);
        for (column, dest) in (0..self.seek_def.skip).zip(self.start_reg..) {
            self.program.emit_insn(Insn::Column {
                cursor_id: self.seek_cursor_id,
                column,
                dest,
                default: None,
            });
        }
        self.skip_next = Some(skip_next);
    }

    /// Emit the start bound and position the cursor at the first candidate row.
    fn emit_start_bound(&mut self, use_bloom_filter: bool) -> Result<()> {
        if self.seek_def.skip > 0 {
            self.emit_skip_scan_start();
        } else if self.seek_def.prefix.is_empty()
            && matches!(self.seek_def.start.last_component, SeekKeyComponent::None)
        {
            match self.seek_def.iter_dir {
                IterationDirection::Forwards => {
                    if self.seek_index.is_some_and(|index| {
                        index.columns[0].effective_nulls_order() == NullsOrder::First
                    }) {
                        self.program.emit_null(self.start_reg, None);
                        self.program.emit_insn(Insn::SeekGT {
                            is_index: self.is_index,
                            cursor_id: self.seek_cursor_id,
                            start_reg: self.start_reg,
                            num_regs: 1,
                            target_pc: self.loop_end,
                        });
                    } else {
                        self.program.emit_insn(Insn::Rewind {
                            cursor_id: self.seek_cursor_id,
                            pc_if_empty: self.loop_end,
                        });
                    }
                }
                IterationDirection::Backwards => {
                    if self.seek_index.is_some_and(|index| {
                        index.columns[0].effective_nulls_order() == NullsOrder::Last
                    }) {
                        self.program.emit_null(self.start_reg, None);
                        self.program.emit_insn(Insn::SeekLT {
                            is_index: self.is_index,
                            cursor_id: self.seek_cursor_id,
                            start_reg: self.start_reg,
                            num_regs: 1,
                            target_pc: self.loop_end,
                        });
                    } else {
                        self.program.emit_insn(Insn::Last {
                            cursor_id: self.seek_cursor_id,
                            pc_if_empty: self.loop_end,
                        });
                    }
                }
            }
            return Ok(());
        }

        let key_reg = self.key_reg()?;
        for (i, key) in self.seek_def.iter(&self.seek_def.start).enumerate() {
            let reg = key_reg + i;
            match key {
                SeekKeyComponent::Expr(expr) => {
                    translate_expr_no_constant_opt(
                        self.program,
                        Some(self.tables),
                        expr,
                        reg,
                        &self.t_ctx.resolver,
                        NoConstantOptReason::RegisterReuse,
                    )?;
                    // A NULL key can never satisfy `=`, so the loop is done as
                    // soon as one shows up. `IS` matches NULL instead: keep the
                    // NULL in the seek register and let the index comparison
                    // find the rows whose key component is NULL. Amphora: a row
                    // value's later elements only position the seek (decision 14).
                    if !expr.is_nonnull(self.tables)
                        && !self.seek_def.is_null_matching_key_component(i)
                        && i <= self.seek_def.prefix.len()
                    {
                        self.program.emit_insn(Insn::IsNull {
                            reg,
                            target_pc: self.loop_end,
                        });
                    }
                }
                SeekKeyComponent::Null => self.program.emit_null(reg, None),
                // The key iterator yields a row value's elements one by one, and nothing for a
                // key with no last component.
                SeekKeyComponent::None | SeekKeyComponent::Row(_) => {
                    return Err(crate::LimboError::InternalError(
                        "a seek key component the key iterator never yields".to_string(),
                    ));
                }
            }
        }
        let num_regs = self.key_len(&self.seek_def.start)?;
        // Which key components match NULL rather than comparing with `=`; the
        // seek and the bloom-filter probe keep their "NULL key cannot match"
        // shortcut for the rest. A skipped column's NULLs are a value it holds.
        let mut null_matching_bits = BitSet::default();
        for i in 0..num_regs {
            if i.checked_sub(self.seek_def.skip)
                .is_none_or(|pos| self.seek_def.is_null_matching_key_component(pos))
            {
                null_matching_bits.set(i)?;
            }
        }
        let null_matching_mask = NullMatchingMask::from(null_matching_bits);

        if let Some(idx) = self.seek_index {
            encode_seek_keys_for_custom_types(
                self.program,
                self.tables,
                idx,
                key_reg,
                self.seek_def.size(&self.seek_def.start),
                self.seek_def.skip,
                &self.t_ctx.resolver,
            )?;
            let affinities = self.key_affinities(&self.seek_def.start);
            if affinities.chars().any(|c| c != affinity::SQLITE_AFF_BLOB) {
                self.program.emit_insn(Insn::Affinity {
                    start_reg: self.start_reg,
                    count: std::num::NonZeroUsize::new(num_regs).unwrap(),
                    affinities,
                });
            }
            if use_bloom_filter {
                turso_assert!(
                    idx.ephemeral,
                    "bloom filter can only be used with ephemeral indexes"
                );
                // The probe treats a NULL key as "definitely absent", which
                // would skip rows whose key IS NULL. `emit_autoindex` never
                // builds a filter for a NULL-matching seek, so probing one
                // here means the build and probe decisions have diverged.
                turso_assert!(
                    null_matching_mask.is_empty(),
                    "a NULL-matching seek must not probe a bloom filter"
                );
                self.program.emit_insn(Insn::Filter {
                    cursor_id: self.seek_cursor_id,
                    key_reg: self.start_reg,
                    num_keys: num_regs,
                    target_pc: self.loop_end,
                });
            }
        }

        // A skip-scan's seek that finds nothing for one value of its skipped columns goes on to the
        // next value.
        let not_found = self.skip_next.unwrap_or(self.loop_end);
        match self.seek_def.start.op {
            SeekOp::GE { eq_only } => self.program.emit_insn(Insn::SeekGE {
                is_index: self.is_index,
                cursor_id: self.seek_cursor_id,
                start_reg: self.start_reg,
                num_regs,
                target_pc: not_found,
                eq_only,
                null_matching_mask,
            }),
            SeekOp::GT => self.program.emit_insn(Insn::SeekGT {
                is_index: self.is_index,
                cursor_id: self.seek_cursor_id,
                start_reg: self.start_reg,
                num_regs,
                target_pc: not_found,
            }),
            SeekOp::LE { eq_only } => self.program.emit_insn(Insn::SeekLE {
                is_index: self.is_index,
                cursor_id: self.seek_cursor_id,
                start_reg: self.start_reg,
                num_regs,
                target_pc: not_found,
                eq_only,
                null_matching_mask,
            }),
            SeekOp::LT => self.program.emit_insn(Insn::SeekLT {
                is_index: self.is_index,
                cursor_id: self.seek_cursor_id,
                start_reg: self.start_reg,
                num_regs,
                target_pc: not_found,
            }),
        };

        Ok(())
    }

    /// Emit the end bound check and anchor the loop-start label.
    fn emit_termination(&mut self, loop_start: BranchOffset) -> Result<()> {
        if self.seek_def.skip == 0
            && self.seek_def.prefix.is_empty()
            && matches!(self.seek_def.end.last_component, SeekKeyComponent::None)
        {
            self.program.preassign_label_to_next_insn(loop_start);
            match self.seek_def.iter_dir {
                IterationDirection::Forwards => {
                    if self.seek_index.is_some_and(|index| {
                        index.columns[0].effective_nulls_order() == NullsOrder::Last
                    }) {
                        self.program.emit_null(self.start_reg, None);
                        self.program.emit_insn(Insn::IdxGE {
                            cursor_id: self.seek_cursor_id,
                            start_reg: self.start_reg,
                            num_regs: 1,
                            target_pc: self.loop_end,
                        });
                    }
                }
                IterationDirection::Backwards => {
                    if self.seek_index.is_some_and(|index| {
                        index.columns[0].effective_nulls_order() == NullsOrder::First
                    }) {
                        self.program.emit_null(self.start_reg, None);
                        self.program.emit_insn(Insn::IdxLE {
                            cursor_id: self.seek_cursor_id,
                            start_reg: self.start_reg,
                            num_regs: 1,
                            target_pc: self.loop_end,
                        });
                    }
                }
            }
            return Ok(());
        }

        let num_regs = self.key_len(&self.seek_def.end)?;
        let last_reg = self.key_reg()? + self.seek_def.prefix.len();
        match &self.seek_def.end.last_component {
            SeekKeyComponent::Expr(expr) => {
                translate_expr_no_constant_opt(
                    self.program,
                    Some(self.tables),
                    expr,
                    last_reg,
                    &self.t_ctx.resolver,
                    NoConstantOptReason::RegisterReuse,
                )?;
                if let Some(idx) = self.seek_index {
                    let last_col = self
                        .seek_def
                        .skip
                        .checked_add(self.seek_def.prefix.len())
                        .ok_or_else(|| {
                            crate::LimboError::InternalError("seek key too long".to_string())
                        })?;
                    encode_seek_keys_for_custom_types(
                        self.program,
                        self.tables,
                        idx,
                        last_reg,
                        1,
                        last_col,
                        &self.t_ctx.resolver,
                    )?;
                    let affinities = self.key_affinities(&self.seek_def.end);
                    if affinities.chars().any(|c| c != affinity::SQLITE_AFF_BLOB) {
                        self.program.emit_insn(Insn::Affinity {
                            start_reg: self.start_reg,
                            count: std::num::NonZeroUsize::new(num_regs).unwrap(),
                            affinities,
                        });
                    }
                }
                if !expr.is_nonnull(self.tables) {
                    self.program.emit_insn(Insn::IsNull {
                        reg: last_reg,
                        target_pc: self.loop_end,
                    });
                }
            }
            // Amphora: a row value's elements in the registers from the range's on; only a NULL
            // first element leaves no row, as a NULL scalar bound does (decision 14).
            SeekKeyComponent::Row(row) => {
                let too_long = || crate::LimboError::InternalError("seek key too long".to_string());
                for (offset, (expr, _)) in row.iter().enumerate() {
                    translate_expr_no_constant_opt(
                        self.program,
                        Some(self.tables),
                        expr,
                        last_reg.checked_add(offset).ok_or_else(too_long)?,
                        &self.t_ctx.resolver,
                        NoConstantOptReason::RegisterReuse,
                    )?;
                }
                if let Some(idx) = self.seek_index {
                    let first_col = self
                        .seek_def
                        .skip
                        .checked_add(self.seek_def.prefix.len())
                        .ok_or_else(too_long)?;
                    encode_seek_keys_for_custom_types(
                        self.program,
                        self.tables,
                        idx,
                        last_reg,
                        row.len(),
                        first_col,
                        &self.t_ctx.resolver,
                    )?;
                    let affinities = self.key_affinities(&self.seek_def.end);
                    let converts = affinities.chars().any(|c| c != affinity::SQLITE_AFF_BLOB);
                    if let Some(count) = std::num::NonZeroUsize::new(num_regs).filter(|_| converts)
                    {
                        self.program.emit_insn(Insn::Affinity {
                            start_reg: self.start_reg,
                            count,
                            affinities,
                        });
                    }
                }
                if row
                    .first()
                    .is_some_and(|(expr, _)| !expr.is_nonnull(self.tables))
                {
                    self.program.emit_insn(Insn::IsNull {
                        reg: last_reg,
                        target_pc: self.loop_end,
                    });
                }
            }
            SeekKeyComponent::Null => self.program.emit_null(last_reg, None),
            SeekKeyComponent::None => {}
        }

        self.program.preassign_label_to_next_insn(loop_start);
        let mut rowid_reg = None;
        let mut affinity = None;
        if !self.is_index {
            rowid_reg = Some(self.program.alloc_register());
            self.program.emit_insn(Insn::RowId {
                cursor_id: self.seek_cursor_id,
                dest: rowid_reg.unwrap(),
            });

            affinity = if let Some(table_ref) = self
                .tables
                .joined_tables()
                .iter()
                .find(|t| t.columns().iter().any(|c| c.is_rowid_alias()))
            {
                if let Some(rowid_col_idx) =
                    table_ref.columns().iter().position(|c| c.is_rowid_alias())
                {
                    Some(table_ref.columns()[rowid_col_idx].affinity())
                } else {
                    Some(Affinity::Numeric)
                }
            } else {
                Some(Affinity::Numeric)
            };
        }

        // Past a skip-scan's range for one value of its skipped columns, the next value.
        let range_end = self.skip_next.unwrap_or(self.loop_end);
        match (self.is_index, self.seek_def.end.op) {
            (true, SeekOp::GE { .. }) => self.program.emit_insn(Insn::IdxGE {
                cursor_id: self.seek_cursor_id,
                start_reg: self.start_reg,
                num_regs,
                target_pc: range_end,
            }),
            (true, SeekOp::GT) => self.program.emit_insn(Insn::IdxGT {
                cursor_id: self.seek_cursor_id,
                start_reg: self.start_reg,
                num_regs,
                target_pc: range_end,
            }),
            (true, SeekOp::LE { .. }) => self.program.emit_insn(Insn::IdxLE {
                cursor_id: self.seek_cursor_id,
                start_reg: self.start_reg,
                num_regs,
                target_pc: range_end,
            }),
            (true, SeekOp::LT) => self.program.emit_insn(Insn::IdxLT {
                cursor_id: self.seek_cursor_id,
                start_reg: self.start_reg,
                num_regs,
                target_pc: range_end,
            }),
            (false, SeekOp::GE { .. }) => self.program.emit_insn(Insn::Ge {
                lhs: rowid_reg.unwrap(),
                rhs: self.start_reg,
                target_pc: self.loop_end,
                flags: CmpInsFlags::default()
                    .jump_if_null()
                    .with_affinity(affinity.unwrap()),
                collation: self.program.curr_collation(),
            }),
            (false, SeekOp::GT) => self.program.emit_insn(Insn::Gt {
                lhs: rowid_reg.unwrap(),
                rhs: self.start_reg,
                target_pc: self.loop_end,
                flags: CmpInsFlags::default()
                    .jump_if_null()
                    .with_affinity(affinity.unwrap()),
                collation: self.program.curr_collation(),
            }),
            (false, SeekOp::LE { .. }) => self.program.emit_insn(Insn::Le {
                lhs: rowid_reg.unwrap(),
                rhs: self.start_reg,
                target_pc: self.loop_end,
                flags: CmpInsFlags::default()
                    .jump_if_null()
                    .with_affinity(affinity.unwrap()),
                collation: self.program.curr_collation(),
            }),
            (false, SeekOp::LT) => self.program.emit_insn(Insn::Lt {
                lhs: rowid_reg.unwrap(),
                rhs: self.start_reg,
                target_pc: self.loop_end,
                flags: CmpInsFlags::default()
                    .jump_if_null()
                    .with_affinity(affinity.unwrap()),
                collation: self.program.curr_collation(),
            }),
        }
        Ok(())
    }

    pub(super) fn emit(mut self, loop_start: BranchOffset, use_bloom_filter: bool) -> Result<()> {
        self.emit_start_bound(use_bloom_filter)?;
        self.emit_termination(loop_start)
    }
}
