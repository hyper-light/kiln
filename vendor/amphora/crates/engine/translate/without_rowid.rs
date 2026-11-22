//! Mutations of index-btree tables use their complete primary key as the row
//! locator. A stable ephemeral read set prevents key updates from revisiting rows.
use crate::schema::{BTreeCharacteristics, BTreeTable, Index};
use crate::sync::Arc;
use crate::translate::emitter::{emit_check_constraints, emit_program, Resolver};
use crate::translate::expr::{
    emit_returning_results, emit_returning_scan_back, translate_expr, ReturningBufferCtx,
};
use crate::translate::optimizer::optimize_plan;
use crate::translate::plan::{
    DeletePlan, Distinctness, EphemeralRowidMode, JoinOrderMember, Plan, QueryDestination,
    ResultSetColumn, SelectPlan, TableReferences, UpdatePlan,
};
use crate::vdbe::builder::{CursorType, ProgramBuilder};
use crate::vdbe::insn::{to_u32, IdxInsertFlags, InsertFlags, Insn, RegisterOrLiteral};
use crate::vdbe::BranchOffset;
use crate::{Connection, LimboError, Result};
use amphora_engine_parser::ast::{self, Expr, ResolveType};

fn offset(start: usize, n: usize) -> Result<usize> {
    start
        .checked_add(n)
        .ok_or_else(|| LimboError::InternalError("register range overflow".into()))
}
fn internal(message: &str) -> LimboError {
    LimboError::InternalError(message.into())
}

pub(crate) struct IndexCursor {
    pub index: Arc<Index>,
    pub cursor: usize,
}

pub(crate) fn open_indexes(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    table: &BTreeTable,
    db: usize,
) -> Result<Vec<IndexCursor>> {
    let indexes: Vec<_> =
        resolver.with_schema(db, |s| s.get_indices(&table.name).cloned().collect());
    let mut result = Vec::with_capacity(indexes.len());
    for index in indexes {
        let physical = Arc::new(index.physical_index(table)?);
        let cursor = program.alloc_cursor_index(None, &physical)?;
        program.emit_insn(Insn::OpenWrite {
            cursor_id: cursor,
            root_page: RegisterOrLiteral::Literal(index.root_page),
            db,
        });
        result.push(IndexCursor { index, cursor });
    }
    Ok(result)
}

/// Evaluate a schema expression against a packed DML image, retaining column
/// affinity and collation through the resolver's register metadata.
fn image_expr(
    program: &mut ProgramBuilder,
    resolver: &mut Resolver,
    table: &BTreeTable,
    start: usize,
    expr: &Expr,
    dest: usize,
) -> Result<()> {
    let layout = table.column_layout()?;
    let mut expr = expr.clone();
    crate::translate::expr::walk_expr_mut(&mut expr, &mut |expr| {
        let pos = match expr {
            Expr::Id(name) => table.get_column(name.as_str()).map(|(pos, _)| pos),
            Expr::Qualified(_, name) | Expr::DoublyQualified(_, _, name) => {
                table.get_column(name.as_str()).map(|(pos, _)| pos)
            }
            Expr::Column { column, .. } => Some(*column),
            _ => None,
        };
        if let Some(pos) = pos {
            let col = table
                .columns()
                .get(pos)
                .ok_or_else(|| internal("index column missing"))?;
            let reg = layout.to_register(start, pos);
            resolver.register_affinities.insert(reg, col.affinity());
            resolver.register_collations.insert(reg, col.collation());
            *expr = Expr::Register(reg);
        }
        Ok(crate::translate::expr::WalkControl::Continue)
    })?;
    translate_expr(program, None, &expr, dest, resolver)?;
    Ok(())
}

pub(crate) fn index_key(
    program: &mut ProgramBuilder,
    resolver: &mut Resolver,
    table: &BTreeTable,
    image: usize,
    index: &Index,
) -> Result<(usize, usize)> {
    let columns = index.physical_columns(table)?;
    let start = program.alloc_registers(columns.len());
    let layout = table.column_layout()?;
    for (i, col) in columns.iter().enumerate() {
        let dest = offset(start, i)?;
        if let Some(expr) = &col.expr {
            image_expr(program, resolver, table, image, expr, dest)?;
        } else {
            program.emit_insn(Insn::Copy {
                src_reg: layout.to_register(image, col.pos_in_table),
                dst_reg: dest,
                extra_amount: 0,
            });
        }
    }
    Ok((start, columns.len()))
}

pub(crate) fn partial_skip(
    program: &mut ProgramBuilder,
    resolver: &mut Resolver,
    table: &BTreeTable,
    image: usize,
    index: &Index,
    skip: BranchOffset,
) -> Result<()> {
    if let Some(expr) = &index.where_clause {
        let reg = program.alloc_register();
        image_expr(program, resolver, table, image, expr, reg)?;
        program.emit_insn(Insn::IfNot {
            reg,
            target_pc: skip,
            jump_if_null: true,
        });
    }
    Ok(())
}

pub(crate) fn delete_indexes(
    program: &mut ProgramBuilder,
    resolver: &mut Resolver,
    table: &BTreeTable,
    image: usize,
    indexes: &[IndexCursor],
) -> Result<()> {
    for item in indexes {
        let skip = program.allocate_label();
        partial_skip(program, resolver, table, image, &item.index, skip)?;
        let (start, count) = index_key(program, resolver, table, image, &item.index)?;
        program.emit_insn(Insn::IdxDelete {
            start_reg: start,
            num_regs: count,
            cursor_id: item.cursor,
            raise_error_if_no_matching_entry: true,
        });
        program.preassign_label_to_next_insn(skip);
    }
    Ok(())
}

pub(crate) fn insert_indexes(
    program: &mut ProgramBuilder,
    resolver: &mut Resolver,
    table: &BTreeTable,
    image: usize,
    indexes: &[IndexCursor],
) -> Result<()> {
    for item in indexes {
        let skip = program.allocate_label();
        partial_skip(program, resolver, table, image, &item.index, skip)?;
        let (start, count) = index_key(program, resolver, table, image, &item.index)?;
        let record = program.alloc_register();
        program.emit_insn(Insn::MakeRecord {
            start_reg: to_u32(start),
            count: to_u32(count),
            dest_reg: to_u32(record),
            index_name: Some(item.index.name.clone()),
            affinity_str: None,
        });
        program.emit_insn(Insn::IdxInsert {
            cursor_id: item.cursor,
            record_reg: record,
            unpacked_start: Some(start),
            unpacked_count: Some(to_u32(count)),
            flags: IdxInsertFlags::new(),
        });
        program.preassign_label_to_next_insn(skip);
    }
    Ok(())
}

pub(crate) fn pk_registers(
    program: &mut ProgramBuilder,
    table: &BTreeTable,
    image: usize,
) -> Result<usize> {
    let start = program.alloc_registers(table.primary_key_columns.len());
    let layout = table.column_layout()?;
    for (i, (name, _)) in table.primary_key_columns.iter().enumerate() {
        let (pos, _) = table
            .get_column(name)
            .ok_or_else(|| internal("primary key column missing"))?;
        program.emit_insn(Insn::Copy {
            src_reg: layout.to_register(image, pos),
            dst_reg: offset(start, i)?,
            extra_amount: 0,
        });
    }
    Ok(start)
}

pub(crate) fn read_image(
    program: &mut ProgramBuilder,
    table: &BTreeTable,
    cursor: usize,
) -> Result<usize> {
    let image = program.alloc_registers(table.columns().len());
    let layout = table.column_layout()?;
    for (i, _) in table.columns().iter().enumerate() {
        program.emit_column_or_rowid(cursor, i, layout.to_register(image, i));
    }
    Ok(image)
}

pub(crate) fn halt_constraint(
    program: &mut ProgramBuilder,
    table: &BTreeTable,
    index: Option<&Index>,
    conflict: ResolveType,
    force_abort: bool,
) {
    let names = if let Some(index) = index {
        index
            .columns
            .iter()
            .map(|c| format!("{}.{}", table.name, c.name))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        table
            .primary_key_columns
            .iter()
            .map(|(name, _)| format!("{}.{}", table.name, name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (description, on_error) = crate::translate::insert::halt_desc_and_on_error(
        &names,
        conflict,
        program.flags.has_statement_conflict(),
    );
    let (description, on_error) = if force_abort {
        (
            format!("UNIQUE constraint failed: {names}"),
            Some(ResolveType::Abort),
        )
    } else {
        (description, on_error)
    };
    program.emit_insn(Insn::Halt {
        err_code: if index.is_some() {
            crate::error::SQLITE_CONSTRAINT_UNIQUE
        } else {
            crate::error::SQLITE_CONSTRAINT_PRIMARYKEY
        },
        description,
        on_error,
        description_reg: None,
    });
}

/// The cursor is positioned on a conflicting row. REPLACE removes every
/// secondary key before deleting that row; it never leaves orphan index entries.
#[allow(clippy::too_many_arguments)]
pub(crate) fn delete_current(
    program: &mut ProgramBuilder,
    resolver: &mut Resolver,
    connection: &Arc<Connection>,
    table: &Arc<BTreeTable>,
    cursor: usize,
    indexes: &[IndexCursor],
    db: usize,
    update: bool,
) -> Result<()> {
    check_supported(table, resolver, db, ast::TriggerEvent::Delete)?;
    let image = read_image(program, table, cursor)?;
    let dummy = pk_registers(program, table, image)?;
    let fk = if connection.foreign_keys_enabled() {
        crate::translate::emitter::delete::emit_fk_child_decrement_on_delete(
            program,
            table,
            &table.name,
            cursor,
            dummy,
            db,
            resolver,
        )?;
        Some(
            crate::translate::fkeys::ForeignKeyActions::prepare_fk_delete_actions(
                program,
                resolver,
                &table.name,
                cursor,
                dummy,
                None,
                db,
            )?,
        )
    } else {
        None
    };
    delete_indexes(program, resolver, table, image, indexes)?;
    program.emit_insn(Insn::Delete {
        cursor_id: cursor,
        table_name: table.name.clone(),
        is_part_of_update: update,
    });
    if let Some(fk) = fk {
        fk.fire_prepared_fk_delete_actions(program, resolver, connection, db)?;
    }
    Ok(())
}

/// Check all keys before changing any index. `old` identifies the same row for
/// UPDATE, so retaining a UNIQUE value does not conflict with itself.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_constraints(
    program: &mut ProgramBuilder,
    resolver: &mut Resolver,
    connection: &Arc<Connection>,
    table: &Arc<BTreeTable>,
    cursor: usize,
    image: usize,
    old: Option<usize>,
    indexes: &[IndexCursor],
    conflict: ResolveType,
    skip: BranchOffset,
    db: usize,
    force_abort: bool,
) -> Result<()> {
    let mut replacements = Vec::new();
    let replace_flag = program.alloc_register();
    program.emit_bool(false, replace_flag);
    let pk_count = table.primary_key_columns.len();
    let key = pk_registers(program, table, image)?;
    let ok = program.allocate_label();
    program.emit_insn(Insn::NoConflict {
        cursor_id: cursor,
        target_pc: ok,
        record_reg: key,
        num_regs: pk_count,
    });
    if let Some(old) = old {
        let old_key = pk_registers(program, table, old)?;
        let different = program.allocate_label();
        for (i, (name, _)) in table.primary_key_columns.iter().enumerate() {
            let (_, col) = table
                .get_column(name)
                .ok_or_else(|| internal("primary key missing"))?;
            program.emit_insn(Insn::Ne {
                lhs: offset(key, i)?,
                rhs: offset(old_key, i)?,
                target_pc: different,
                flags: Default::default(),
                collation: Some(
                    table
                        .primary_key_constraint_collation(name)
                        .unwrap_or_else(|| col.collation()),
                ),
            });
        }
        program.emit_insn(Insn::Goto { target_pc: ok });
        program.preassign_label_to_next_insn(different);
    }
    let pk_conflict = if force_abort || program.flags.has_statement_conflict() {
        conflict
    } else {
        primary_key_policy(table)
    };
    match pk_conflict {
        ResolveType::Ignore => {
            program.emit_insn(Insn::Goto { target_pc: skip });
        }
        ResolveType::Replace => {
            replacements.push(queue_replace(program, table, cursor, replace_flag)?)
        }
        _ => halt_constraint(program, table, None, pk_conflict, force_abort),
    }
    program.preassign_label_to_next_insn(ok);
    for item in indexes.iter().filter(|i| i.index.unique) {
        let replace_flag = program.alloc_register();
        program.emit_bool(false, replace_flag);
        let ok = program.allocate_label();
        partial_skip(program, resolver, table, image, &item.index, ok)?;
        let (key, _) = index_key(program, resolver, table, image, &item.index)?;
        program.emit_insn(Insn::NoConflict {
            cursor_id: item.cursor,
            target_pc: ok,
            record_reg: key,
            num_regs: item.index.columns.len(),
        });
        // Read the conflicting primary key from the secondary index's physical suffix.
        let physical = item.index.physical_columns(table)?;
        let conflicting_pk = program.alloc_registers(pk_count);
        for (i, (name, _)) in table.primary_key_columns.iter().enumerate() {
            let (pos, col) = table
                .get_column(name)
                .ok_or_else(|| internal("primary key missing"))?;
            let physical_pos = physical
                .iter()
                .position(|c| {
                    c.pos_in_table == pos
                        && c.expr.is_none()
                        && c.collation.unwrap_or_else(|| col.collation())
                            == table
                                .primary_key_constraint_collation(name)
                                .unwrap_or_else(|| col.collation())
                })
                .ok_or_else(|| internal("index primary key suffix missing"))?;
            program.emit_column_or_rowid(item.cursor, physical_pos, offset(conflicting_pk, i)?);
        }
        if let Some(old) = old {
            let old_key = pk_registers(program, table, old)?;
            let different = program.allocate_label();
            for (i, (name, _)) in table.primary_key_columns.iter().enumerate() {
                let (_, col) = table
                    .get_column(name)
                    .ok_or_else(|| internal("primary key missing"))?;
                program.emit_insn(Insn::Ne {
                    lhs: offset(conflicting_pk, i)?,
                    rhs: offset(old_key, i)?,
                    target_pc: different,
                    flags: Default::default(),
                    collation: Some(
                        table
                            .primary_key_constraint_collation(name)
                            .unwrap_or_else(|| col.collation()),
                    ),
                });
            }
            program.emit_insn(Insn::Goto { target_pc: ok });
            program.preassign_label_to_next_insn(different);
        }
        let effective = if force_abort || program.flags.has_statement_conflict() {
            conflict
        } else {
            item.index.on_conflict.unwrap_or(conflict)
        };
        match effective {
            ResolveType::Ignore => {
                program.emit_insn(Insn::Goto { target_pc: skip });
            }
            ResolveType::Replace => {
                program.emit_insn(Insn::NoConflict {
                    cursor_id: cursor,
                    target_pc: ok,
                    record_reg: conflicting_pk,
                    num_regs: pk_count,
                });
                replacements.push(queue_replace(program, table, cursor, replace_flag)?);
            }
            _ => halt_constraint(program, table, Some(&item.index), effective, force_abort),
        }
        program.preassign_label_to_next_insn(ok);
    }
    apply_replacements(
        program,
        resolver,
        connection,
        table,
        cursor,
        indexes,
        db,
        &replacements,
    )?;
    Ok(())
}

pub(crate) fn write_row(
    program: &mut ProgramBuilder,
    table: &BTreeTable,
    cursor: usize,
    image: usize,
) -> Result<()> {
    // The on-disk index record is in PK-first order, not declaration order.
    let count = table.columns().len();
    let physical = program.alloc_registers(count);
    let layout = table.column_layout()?;
    for (i, _) in table.columns().iter().enumerate() {
        let pos = table
            .logical_to_physical_map
            .get(i)
            .copied()
            .ok_or_else(|| internal("physical column missing"))?;
        program.emit_insn(Insn::Copy {
            src_reg: layout.to_register(image, i),
            dst_reg: offset(physical, pos)?,
            extra_amount: 0,
        });
    }
    let record = program.alloc_register();
    program.emit_insn(Insn::MakeRecord {
        start_reg: to_u32(physical),
        count: to_u32(count),
        dest_reg: to_u32(record),
        index_name: None,
        affinity_str: None,
    });
    let dummy = program.alloc_register();
    program.emit_null(dummy, None);
    program.emit_insn(Insn::Insert {
        cursor,
        key_reg: dummy,
        record_reg: record,
        flag: InsertFlags::new().require_seek(),
        table_name: table.name.clone(),
    });
    Ok(())
}

fn scratch_table(table: &BTreeTable) -> Arc<BTreeTable> {
    Arc::new(BTreeTable::new(
        0,
        "without_rowid_write_set".into(),
        vec![],
        table.columns().to_vec(),
        BTreeCharacteristics::HAS_ROWID,
        vec![],
        vec![],
        vec![],
        None,
    ))
}

fn select_plan(
    mut tables: TableReferences,
    columns: Vec<ResultSetColumn>,
    where_clause: Vec<crate::translate::plan::WhereTerm>,
    subqueries: Vec<crate::translate::plan::NonFromClauseSubquery>,
    destination: QueryDestination,
) -> SelectPlan {
    for column in &columns {
        if let Expr::Column { table, column, .. } = &column.expr {
            tables.mark_column_used(*table, *column);
        }
    }
    let join_order = tables
        .joined_tables()
        .iter()
        .enumerate()
        .map(|(i, table)| JoinOrderMember {
            table_id: table.internal_id,
            original_idx: i,
            is_outer: false,
        })
        .collect();
    SelectPlan {
        table_references: tables,
        join_order,
        result_columns: columns,
        where_clause,
        group_by: None,
        order_by: vec![],
        aggregates: vec![],
        limit: None,
        offset: None,
        contains_constant_false_condition: false,
        query_destination: destination,
        distinctness: Distinctness::NonDistinct,
        values: vec![],
        window: None,
        non_from_clause_subqueries: subqueries,
        input_cardinality_hint: None,
        estimated_output_rows: None,
        estimated_cost: None,
        simple_aggregate: None,
        phantom_params: vec![],
    }
}

fn open_table(program: &mut ProgramBuilder, table: &Arc<BTreeTable>, db: usize) -> usize {
    let cursor = program.alloc_cursor_id(CursorType::BTreeTable(table.clone()));
    program.emit_insn(Insn::OpenWrite {
        cursor_id: cursor,
        root_page: RegisterOrLiteral::Literal(table.root_page),
        db,
    });
    cursor
}

fn check_supported(
    table: &BTreeTable,
    resolver: &Resolver,
    db: usize,
    event: ast::TriggerEvent,
) -> Result<()> {
    if has_encoded_columns(table, resolver, db) {
        crate::bail_parse_error!(
            "custom-type and array mutations of WITHOUT ROWID tables are not supported"
        );
    }
    if !resolver.with_schema(db, |schema| {
        schema
            .get_dependent_materialized_views(&table.name)
            .is_empty()
    }) {
        crate::bail_parse_error!("materialized views on WITHOUT ROWID tables are not supported");
    }
    if table.has_virtual_columns {
        crate::bail_parse_error!(
            "generated-column mutations of WITHOUT ROWID tables are not supported"
        );
    }
    if crate::translate::trigger_exec::has_triggers_including_temp(resolver, db, event, None, table)
    {
        crate::bail_parse_error!("triggered mutations of WITHOUT ROWID tables are not supported");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_image(
    program: &mut ProgramBuilder,
    resolver: &mut Resolver,
    connection: &Arc<Connection>,
    table: &Arc<BTreeTable>,
    image: usize,
    dummy: usize,
    conflict: ResolveType,
    skip: BranchOffset,
    refs: &TableReferences,
    force_abort: bool,
) -> Result<()> {
    let layout = table.column_layout()?;
    for (i, col) in table.columns().iter().enumerate() {
        let reg = layout.to_register(image, i);
        program.emit_column_affinity(reg, col.affinity());
        if col.notnull() {
            let notnull_conflict = if force_abort || program.flags.has_statement_conflict() {
                conflict
            } else {
                col.notnull_conflict_clause.unwrap_or(ResolveType::Abort)
            };
            if notnull_conflict == ResolveType::Ignore {
                program.emit_insn(Insn::IsNull {
                    reg,
                    target_pc: skip,
                });
            } else {
                if notnull_conflict == ResolveType::Replace {
                    if let Some(default) = &col.default {
                        let nonnull = program.allocate_label();
                        program.emit_insn(Insn::NotNull {
                            reg,
                            target_pc: nonnull,
                        });
                        crate::translate::expr::translate_expr_no_constant_opt(
                            program,
                            None,
                            default,
                            reg,
                            resolver,
                            crate::translate::expr::NoConstantOptReason::RegisterReuse,
                        )?;
                        program.emit_column_affinity(reg, col.affinity());
                        program.preassign_label_to_next_insn(nonnull);
                    }
                }
                if force_abort {
                    let nonnull = program.allocate_label();
                    program.emit_insn(Insn::NotNull {
                        reg,
                        target_pc: nonnull,
                    });
                    program.emit_insn(Insn::Halt {
                        err_code: crate::error::SQLITE_CONSTRAINT_NOTNULL,
                        description: format!(
                            "NOT NULL constraint failed: {}.{}",
                            table.name,
                            col.name.as_deref().unwrap_or("")
                        ),
                        on_error: Some(ResolveType::Abort),
                        description_reg: None,
                    });
                    program.preassign_label_to_next_insn(nonnull);
                } else {
                    program.emit_insn(Insn::HaltIfNull {
                        target_reg: reg,
                        err_code: crate::error::SQLITE_CONSTRAINT_NOTNULL,
                        description: format!(
                            "{}.{}",
                            table.name,
                            col.name.as_deref().unwrap_or("")
                        ),
                    });
                }
            }
        }
    }
    if table.is_strict {
        program.emit_insn(Insn::TypeCheck {
            start_reg: image,
            count: table.columns().len(),
            check_generated: true,
            table_reference: BTreeTable::type_check_table_ref(table, resolver.schema()),
        });
    }
    let first_check_instruction = program.insns.len();
    emit_check_constraints(
        program,
        &table.check_constraints,
        resolver,
        &table.name,
        dummy,
        table.columns().iter().enumerate().filter_map(|(i, col)| {
            col.name
                .as_deref()
                .map(|name| (name, layout.to_register(image, i)))
        }),
        connection,
        conflict,
        skip,
        Some(refs),
    )?;
    if force_abort {
        for (instruction, _) in program.insns.iter_mut().skip(first_check_instruction) {
            if let Insn::Halt {
                err_code, on_error, ..
            } = instruction
            {
                if *err_code != 0 {
                    *on_error = Some(ResolveType::Abort);
                }
            }
        }
    }
    Ok(())
}

fn returning_buffer(
    program: &mut ProgramBuilder,
    table: &Arc<BTreeTable>,
    count: usize,
) -> Option<ReturningBufferCtx> {
    if count == 0 {
        return None;
    }
    let cursor_id = program.alloc_cursor_id(CursorType::BTreeTable(scratch_table(table)));
    program.emit_insn(Insn::OpenEphemeral {
        cursor_id,
        is_table: true,
    });
    Some(ReturningBufferCtx {
        cursor_id,
        num_columns: count,
    })
}

pub(crate) fn emit_update(
    connection: &Arc<Connection>,
    resolver: &Resolver,
    program: &mut ProgramBuilder,
    mut update: UpdatePlan,
) -> Result<()> {
    let target = update.target_table.clone();
    let table = target
        .table
        .btree()
        .ok_or_else(|| internal("update target is not btree"))?;
    let db = target.database_id;
    check_supported(&table, resolver, db, ast::TriggerEvent::Update)?;
    if program.capture_data_changes_info().is_some() {
        crate::bail_parse_error!("CDC on WITHOUT ROWID tables is not supported");
    }
    let mut resolver = resolver.fork();
    let conflict = update.or_conflict.unwrap_or(ResolveType::Abort);
    program.set_resolve_type(conflict);
    program
        .flags
        .set_has_statement_conflict(update.or_conflict.is_some());
    // Multiple-row mutations can fail after writing an earlier row.
    program.set_multi_write(true);
    program.set_may_abort(true);
    let refs = TableReferences::new(
        vec![target.clone()],
        update.from_tables.outer_query_refs().to_vec(),
    );
    if !update.from_tables.joined_tables().is_empty() {
        crate::bail_parse_error!("UPDATE FROM of WITHOUT ROWID tables is not supported");
    }
    let columns = primary_key_columns(&table, target.internal_id, db)?;
    let scratch = scratch_table(&table);
    let read_cursor = program.alloc_cursor_id(CursorType::BTreeTable(scratch.clone()));
    program.emit_insn(Insn::OpenEphemeral {
        cursor_id: read_cursor,
        is_table: true,
    });
    let tables = update.build_read_scope_tables();
    let mut read_subqueries = Vec::new();
    let mut write_subqueries = Vec::new();
    for subquery in update.non_from_clause_subqueries.drain(..) {
        if matches!(
            subquery.eval_phase,
            crate::translate::plan::SubqueryEvalPhase::PreWrite
                | crate::translate::plan::SubqueryEvalPhase::PostWriteReturning
        ) {
            write_subqueries.push(subquery);
        } else {
            read_subqueries.push(subquery);
        }
    }
    let mut select = Plan::Select(Box::new(select_plan(
        tables,
        columns,
        std::mem::take(&mut update.where_clause),
        read_subqueries,
        QueryDestination::EphemeralTable {
            cursor_id: read_cursor,
            table: scratch,
            rowid_mode: EphemeralRowidMode::Auto,
        },
    )));
    optimize_plan(program, &mut select, &resolver)?;
    program.nested(|program| emit_program(connection, &resolver, program, select, |_| {}))?;
    let cursor = open_table(program, &table, db);
    let indexes = open_indexes(program, &resolver, &table, db)?;
    let result_columns = update.returning.take().unwrap_or_default();
    let buffer = returning_buffer(program, &table, result_columns.len());
    let end = program.allocate_label();
    let next = program.allocate_label();
    let body = program.allocate_label();
    program.emit_insn(Insn::Rewind {
        cursor_id: read_cursor,
        pc_if_empty: end,
    });
    program.preassign_label_to_next_insn(body);
    let key_image = read_primary_key_image(program, &mut resolver, &table, read_cursor)?;
    let old_pk = pk_registers(program, &table, key_image)?;
    program.emit_insn(Insn::NoConflict {
        cursor_id: cursor,
        target_pc: next,
        record_reg: old_pk,
        num_regs: table.primary_key_columns.len(),
    });
    let old = read_image(program, &table, cursor)?;
    let new = program.alloc_registers(table.columns().len());
    let layout = table.column_layout()?;
    for (i, _) in table.columns().iter().enumerate() {
        program.emit_insn(Insn::Copy {
            src_reg: layout.to_register(old, i),
            dst_reg: layout.to_register(new, i),
            extra_amount: 0,
        });
    }
    let mut updated = crate::translate::plan::ColumnMask::default();
    for set in &update.set_clauses {
        updated.set(set.column_index)?;
    }
    let overrides = [(
        crate::vdbe::builder::CursorKey::table(target.internal_id),
        cursor,
    )];
    program.with_cursor_overrides(&overrides, |program| {
        emit_write_subqueries(
            program,
            &resolver,
            &mut write_subqueries,
            false,
            db,
            &table.name,
        )?;
        for set in &update.set_clauses {
            crate::translate::expr::translate_expr_no_constant_opt(
                program,
                Some(&refs),
                &set.expr,
                layout.to_register(new, set.column_index),
                &resolver,
                crate::translate::expr::NoConstantOptReason::RegisterReuse,
            )?;
        }
        Ok(())
    })?;
    crate::translate::expr::emit_custom_type_encode_columns(
        program,
        &resolver,
        table.columns(),
        new,
        Some(&updated),
        &table.name,
        &layout,
    )?;
    let new_pk = pk_registers(program, &table, new)?;
    validate_image(
        program,
        &mut resolver,
        connection,
        &table,
        new,
        new_pk,
        conflict,
        next,
        &refs,
        false,
    )?;
    let new_pk = pk_registers(program, &table, new)?;
    check_constraints(
        program,
        &mut resolver,
        connection,
        &table,
        cursor,
        new,
        Some(old),
        &indexes,
        conflict,
        next,
        db,
        false,
    )?;
    // Constraint probes and REPLACE can reposition the table cursor.
    program.emit_insn(Insn::NoConflict {
        cursor_id: cursor,
        target_pc: next,
        record_reg: old_pk,
        num_regs: table.primary_key_columns.len(),
    });
    let affected = if connection.foreign_keys_enabled() {
        let affected = crate::translate::fkeys::affected_parent_fks_for_update(
            &resolver, &table, &updated, db,
        )?;
        crate::translate::fkeys::emit_fk_child_update_counters(
            program,
            &table,
            &table.name,
            cursor,
            new,
            new_pk,
            &updated,
            db,
            &resolver,
            &layout,
        )?;
        crate::translate::fkeys::emit_fk_update_parent_actions(
            program,
            &table,
            &affected,
            indexes.iter().map(|i| &i.index),
            cursor,
            old_pk,
            new,
            new_pk,
            None,
            &updated,
            crate::translate::fkeys::ParentKeyNewProbeMode::BeforeWrite,
            db,
            &resolver,
        )?;
        affected
    } else {
        crate::alloc::Vec::new()
    };
    delete_indexes(program, &mut resolver, &table, old, &indexes)?;
    program.emit_insn(Insn::Delete {
        cursor_id: cursor,
        table_name: table.name.clone(),
        is_part_of_update: true,
    });
    insert_indexes(program, &mut resolver, &table, new, &indexes)?;
    write_row(program, &table, cursor, new)?;
    if connection.foreign_keys_enabled() {
        let identity_old = program.alloc_registers(table.columns().len());
        for (i, _) in table.columns().iter().enumerate() {
            program.emit_insn(Insn::Copy {
                src_reg: layout.to_register(old, i),
                dst_reg: offset(identity_old, i)?,
                extra_amount: 0,
            });
        }
        crate::translate::fkeys::fire_fk_update_actions(
            program,
            &mut resolver,
            &table.name,
            old_pk,
            identity_old,
            new,
            new_pk,
            connection,
            db,
            &affected,
        )?;
    }
    let returning_cache = crate::translate::expr::seed_returning_row_image_in_cache(
        program,
        &refs,
        new,
        new_pk,
        &mut resolver,
        &layout,
    )?;
    program.with_cursor_overrides(&overrides, |program| {
        emit_write_subqueries(
            program,
            &resolver,
            &mut write_subqueries,
            true,
            db,
            &table.name,
        )
    })?;
    crate::translate::expr::restore_returning_row_image_in_cache(&mut resolver, returning_cache);
    emit_returning_results(
        program,
        &refs,
        &result_columns,
        new,
        new_pk,
        &mut resolver,
        buffer.as_ref(),
        &layout,
    )?;
    program.preassign_label_to_next_insn(next);
    program.emit_insn(Insn::Next {
        cursor_id: read_cursor,
        pc_if_next: body,
        fullscan: false,
        is_index: false,
    });
    program.preassign_label_to_next_insn(end);
    if let Some(buffer) = buffer {
        program.emit_insn(Insn::FkCheck { deferred: false });
        emit_returning_scan_back(program, &buffer);
    }
    program.result_columns = result_columns;
    program.table_references.extend(refs);
    Ok(())
}

pub(crate) fn emit_delete(
    connection: &Arc<Connection>,
    resolver: &Resolver,
    program: &mut ProgramBuilder,
    mut delete: DeletePlan,
) -> Result<()> {
    let target = delete
        .table_references
        .joined_tables()
        .first()
        .cloned()
        .ok_or_else(|| internal("delete target missing"))?;
    let table = target
        .table
        .btree()
        .ok_or_else(|| internal("delete target not btree"))?;
    let db = target.database_id;
    check_supported(&table, resolver, db, ast::TriggerEvent::Delete)?;
    if program.capture_data_changes_info().is_some() {
        crate::bail_parse_error!("CDC on WITHOUT ROWID tables is not supported");
    }
    program.set_multi_write(true);
    program.set_may_abort(true);
    let mut resolver = resolver.fork();
    let scratch = scratch_table(&table);
    let read_cursor = program.alloc_cursor_id(CursorType::BTreeTable(scratch.clone()));
    program.emit_insn(Insn::OpenEphemeral {
        cursor_id: read_cursor,
        is_table: true,
    });
    let destination = QueryDestination::EphemeralTable {
        cursor_id: read_cursor,
        table: scratch,
        rowid_mode: EphemeralRowidMode::Auto,
    };
    let mut returning_subqueries = Vec::new();
    let mut read_subqueries = Vec::new();
    for subquery in delete.non_from_clause_subqueries.drain(..) {
        if subquery.is_post_write_returning() {
            returning_subqueries.push(subquery);
        } else {
            read_subqueries.push(subquery);
        }
    }
    let mut select = if let Some(mut select) = delete.rowset_plan.take() {
        select.result_columns = primary_key_columns(&table, target.internal_id, db)?;
        select.query_destination = destination;
        Plan::Select(Box::new(select))
    } else {
        Plan::Select(Box::new(select_plan(
            delete.table_references.clone(),
            primary_key_columns(&table, target.internal_id, db)?,
            std::mem::take(&mut delete.where_clause),
            read_subqueries,
            destination,
        )))
    };
    optimize_plan(program, &mut select, &resolver)?;
    program.nested(|program| emit_program(connection, &resolver, program, select, |_| {}))?;
    let cursor = open_table(program, &table, db);
    let indexes = open_indexes(program, &resolver, &table, db)?;
    let buffer = returning_buffer(program, &table, delete.result_columns.len());
    let end = program.allocate_label();
    let next = program.allocate_label();
    let body = program.allocate_label();
    program.emit_insn(Insn::Rewind {
        cursor_id: read_cursor,
        pc_if_empty: end,
    });
    program.preassign_label_to_next_insn(body);
    let key_image = read_primary_key_image(program, &mut resolver, &table, read_cursor)?;
    let key = pk_registers(program, &table, key_image)?;
    program.emit_insn(Insn::NoConflict {
        cursor_id: cursor,
        target_pc: next,
        record_reg: key,
        num_regs: table.primary_key_columns.len(),
    });
    let image = read_image(program, &table, cursor)?;
    let layout = table.column_layout()?;
    delete_current(
        program,
        &mut resolver,
        connection,
        &table,
        cursor,
        &indexes,
        db,
        false,
    )?;
    let overrides = [(
        crate::vdbe::builder::CursorKey::table(target.internal_id),
        cursor,
    )];
    let cache = crate::translate::expr::seed_returning_row_image_in_cache(
        program,
        &delete.table_references,
        image,
        key,
        &mut resolver,
        &layout,
    )?;
    program.with_cursor_overrides(&overrides, |program| {
        emit_write_subqueries(
            program,
            &resolver,
            &mut returning_subqueries,
            true,
            db,
            &table.name,
        )
    })?;
    crate::translate::expr::restore_returning_row_image_in_cache(&mut resolver, cache);
    emit_returning_results(
        program,
        &delete.table_references,
        &delete.result_columns,
        image,
        key,
        &mut resolver,
        buffer.as_ref(),
        &layout,
    )?;
    program.preassign_label_to_next_insn(next);
    program.emit_insn(Insn::Next {
        cursor_id: read_cursor,
        pc_if_next: body,
        fullscan: false,
        is_index: false,
    });
    program.preassign_label_to_next_insn(end);
    if let Some(buffer) = buffer {
        program.emit_insn(Insn::FkCheck { deferred: false });
        emit_returning_scan_back(program, &buffer);
    }
    program.result_columns = delete.result_columns;
    program.table_references.extend(delete.table_references);
    Ok(())
}

fn raw_column(program: &mut ProgramBuilder, cursor_id: usize, column: usize, dest: usize) {
    program.emit_insn(Insn::Column {
        cursor_id,
        column,
        dest,
        default: None,
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_upsert(
    program: &mut ProgramBuilder,
    table: &crate::schema::Table,
    ctx: &crate::translate::insert::InsertEmitCtx,
    insertion: &crate::translate::insert::Insertion,
    sets: &mut [(usize, Box<Expr>)],
    predicate: &mut Option<Box<Expr>>,
    resolver: &mut Resolver,
    returning: &mut [ResultSetColumn],
    connection: &Arc<Connection>,
    refs: &mut TableReferences,
    alias: Option<&str>,
) -> Result<()> {
    let statement_conflict = program.flags.has_statement_conflict();
    program.flags.set_has_statement_conflict(true);
    let bt = ctx.table;
    check_supported(bt, resolver, ctx.database_id, ast::TriggerEvent::Update)?;
    let layout = bt.column_layout()?;
    let old = read_image(program, bt, ctx.cursor_id)?;
    let new = program.alloc_registers(bt.columns().len());
    let old_pk = pk_registers(program, bt, old)?;
    for (i, col) in bt.columns().iter().enumerate() {
        let reg = layout.to_register(old, i);
        resolver.register_affinities.insert(reg, col.affinity());
        resolver.register_collations.insert(reg, col.collation());
        program.emit_insn(Insn::Copy {
            src_reg: reg,
            dst_reg: layout.to_register(new, i),
            extra_amount: 0,
        });
    }
    if let Some(predicate) = predicate {
        crate::translate::upsert::rewrite_expr_to_registers(
            predicate,
            table,
            old,
            old_pk,
            Some(table.get_name()),
            alias,
            Some(insertion),
            true,
            None,
            &layout,
        )?;
        let value = program.alloc_register();
        translate_expr(program, None, predicate, value, resolver)?;
        program.emit_insn(Insn::IfNot {
            reg: value,
            target_pc: ctx.loop_labels.row_done,
            jump_if_null: true,
        });
    }
    let mut updated = crate::translate::plan::ColumnMask::default();
    for (pos, expr) in sets {
        crate::translate::upsert::rewrite_expr_to_registers(
            expr,
            table,
            old,
            old_pk,
            Some(table.get_name()),
            alias,
            Some(insertion),
            true,
            None,
            &layout,
        )?;
        crate::translate::expr::translate_expr_no_constant_opt(
            program,
            None,
            expr,
            layout.to_register(new, *pos),
            resolver,
            crate::translate::expr::NoConstantOptReason::RegisterReuse,
        )?;
        updated.set(*pos)?;
    }
    let new_pk = pk_registers(program, bt, new)?;
    // SQLite specifies that the DO UPDATE arm always uses ABORT, including when
    // the enclosing INSERT selected another conflict algorithm.
    validate_image(
        program,
        resolver,
        connection,
        bt,
        new,
        new_pk,
        ResolveType::Abort,
        ctx.loop_labels.row_done,
        refs,
        true,
    )?;
    let indexes = crate::translate::insert::without_rowid_index_cursors(ctx, resolver)?;
    let new_pk = pk_registers(program, bt, new)?;
    check_constraints(
        program,
        resolver,
        connection,
        bt,
        ctx.cursor_id,
        new,
        Some(old),
        &indexes,
        ResolveType::Abort,
        ctx.loop_labels.row_done,
        ctx.database_id,
        true,
    )?;
    program.emit_insn(Insn::NoConflict {
        cursor_id: ctx.cursor_id,
        target_pc: ctx.loop_labels.row_done,
        record_reg: old_pk,
        num_regs: bt.primary_key_columns.len(),
    });
    let affected = if connection.foreign_keys_enabled() {
        let affected = crate::translate::fkeys::affected_parent_fks_for_update(
            resolver,
            bt,
            &updated,
            ctx.database_id,
        )?;
        crate::translate::fkeys::emit_fk_child_update_counters(
            program,
            bt,
            &bt.name,
            ctx.cursor_id,
            new,
            new_pk,
            &updated,
            ctx.database_id,
            resolver,
            &layout,
        )?;
        crate::translate::fkeys::emit_fk_update_parent_actions(
            program,
            bt,
            &affected,
            indexes.iter().map(|i| &i.index),
            ctx.cursor_id,
            old_pk,
            new,
            new_pk,
            None,
            &updated,
            crate::translate::fkeys::ParentKeyNewProbeMode::BeforeWrite,
            ctx.database_id,
            resolver,
        )?;
        affected
    } else {
        crate::alloc::Vec::new()
    };
    delete_indexes(program, resolver, bt, old, &indexes)?;
    program.emit_insn(Insn::Delete {
        cursor_id: ctx.cursor_id,
        table_name: bt.name.clone(),
        is_part_of_update: true,
    });
    insert_indexes(program, resolver, bt, new, &indexes)?;
    write_row(program, bt, ctx.cursor_id, new)?;
    if connection.foreign_keys_enabled() {
        let identity_old = program.alloc_registers(bt.columns().len());
        for (i, _) in bt.columns().iter().enumerate() {
            program.emit_insn(Insn::Copy {
                src_reg: layout.to_register(old, i),
                dst_reg: offset(identity_old, i)?,
                extra_amount: 0,
            });
        }
        crate::translate::fkeys::fire_fk_update_actions(
            program,
            resolver,
            &bt.name,
            old_pk,
            identity_old,
            new,
            new_pk,
            connection,
            ctx.database_id,
            &affected,
        )?;
    }
    emit_returning_results(
        program,
        refs,
        returning,
        new,
        new_pk,
        resolver,
        ctx.returning_buffer.as_ref(),
        &layout,
    )?;
    program.emit_insn(Insn::Goto {
        target_pc: ctx.loop_labels.row_done,
    });
    program.flags.set_has_statement_conflict(statement_conflict);
    Ok(())
}

pub(crate) fn primary_key_policy(table: &BTreeTable) -> ResolveType {
    table
        .unique_sets
        .iter()
        .find(|set| set.is_primary_key)
        .and_then(|set| set.conflict_clause)
        .unwrap_or(ResolveType::Abort)
}

pub(crate) struct Replacement {
    flag: usize,
    key: usize,
}

pub(crate) fn queue_replace(
    program: &mut ProgramBuilder,
    table: &BTreeTable,
    cursor: usize,
    flag: usize,
) -> Result<Replacement> {
    let image = read_image(program, table, cursor)?;
    let key = pk_registers(program, table, image)?;
    program.emit_bool(true, flag);
    Ok(Replacement { flag, key })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_replacements(
    program: &mut ProgramBuilder,
    resolver: &mut Resolver,
    connection: &Arc<Connection>,
    table: &Arc<BTreeTable>,
    cursor: usize,
    indexes: &[IndexCursor],
    db: usize,
    replacements: &[Replacement],
) -> Result<()> {
    for replacement in replacements {
        let skip = program.allocate_label();
        program.emit_insn(Insn::IfNot {
            reg: replacement.flag,
            target_pc: skip,
            jump_if_null: true,
        });
        program.emit_insn(Insn::NoConflict {
            cursor_id: cursor,
            target_pc: skip,
            record_reg: replacement.key,
            num_regs: table.primary_key_columns.len(),
        });
        delete_current(
            program, resolver, connection, table, cursor, indexes, db, true,
        )?;
        program.preassign_label_to_next_insn(skip);
    }
    Ok(())
}

fn primary_key_columns(
    table: &BTreeTable,
    id: ast::TableInternalId,
    db: usize,
) -> Result<Vec<ResultSetColumn>> {
    table
        .primary_key_columns
        .iter()
        .map(|(name, _)| {
            let (column, _) = table
                .get_column(name)
                .ok_or_else(|| internal("primary key column missing"))?;
            Ok(ResultSetColumn {
                expr: Expr::Column {
                    database: Some(db),
                    table: id,
                    column,
                    is_rowid_alias: false,
                },
                alias: None,
                implicit_column_name: None,
                contains_aggregates: false,
            })
        })
        .collect()
}

fn read_primary_key_image(
    program: &mut ProgramBuilder,
    resolver: &mut Resolver,
    table: &BTreeTable,
    cursor: usize,
) -> Result<usize> {
    let image = program.alloc_registers(table.columns().len());
    let layout = table.column_layout()?;
    let mut pk_columns = crate::translate::plan::ColumnMask::default();
    for (i, (name, _)) in table.primary_key_columns.iter().enumerate() {
        let (pos, _) = table
            .get_column(name)
            .ok_or_else(|| internal("primary key missing"))?;
        raw_column(program, cursor, i, layout.to_register(image, pos));
        pk_columns.set(pos)?;
    }
    crate::translate::expr::emit_custom_type_encode_columns(
        program,
        resolver,
        table.columns(),
        image,
        Some(&pk_columns),
        &table.name,
        &layout,
    )?;
    Ok(image)
}

fn emit_write_subqueries(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    subqueries: &mut [crate::translate::plan::NonFromClauseSubquery],
    returning: bool,
    database_id: usize,
    table_name: &str,
) -> Result<()> {
    for subquery in subqueries
        .iter_mut()
        .filter(|s| !s.has_been_evaluated() && s.is_post_write_returning() == returning)
    {
        let reads_target = subquery.reads_table(database_id, table_name);
        let plan = subquery.consume_plan(crate::translate::plan::EvalAt::Loop(0));
        let run = if returning {
            if subquery.correlated || reads_target {
                crate::translate::plan::SubqueryRun::EveryRead
            } else {
                crate::translate::plan::SubqueryRun::Once
            }
        } else {
            match (subquery.correlated, &subquery.query_type) {
                (true, ast::SubqueryType::RowValue { num_regs, .. }) if *num_regs > 1 => {
                    crate::translate::plan::SubqueryRun::EveryRow
                }
                (true, _) => crate::translate::plan::SubqueryRun::EveryRead,
                (false, _) => crate::translate::plan::SubqueryRun::Once,
            }
        };
        crate::translate::subquery::emit_non_from_clause_subquery(
            program,
            resolver,
            subquery.internal_id,
            *plan,
            &subquery.query_type,
            run,
            returning,
        )?;
    }
    Ok(())
}

/// These columns require a decode/evaluate/encode pipeline. Rowidless DML
/// refuses until that pipeline is available, rather than evaluating schema
/// expressions against an encoded payload.
pub(crate) fn has_encoded_columns(
    table: &BTreeTable,
    resolver: &Resolver,
    database_id: usize,
) -> bool {
    resolver.with_schema(database_id, |schema| {
        table.columns().iter().any(|column| {
            column.is_array()
                || schema
                    .get_type_def(&column.ty_str, table.is_strict)
                    .is_some()
        })
    })
}
