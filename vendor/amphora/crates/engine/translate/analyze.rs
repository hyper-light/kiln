use crate::sync::Arc;

use crate::{
    bail_parse_error,
    column_stats::{SampleFunction, SketchFunction},
    function::{AccumulatorFunc, AggFunc, ExtFunc, Func, FuncCtx, ScalarFunc},
    schema::{BTreeTable, Index, RESERVED_TABLE_PREFIXES},
    storage::pager::CreateBTreeFlags,
    translate::{
        emitter::Resolver,
        schema::{emit_schema_entry, SchemaEntryType, SQLITE_TABLEID},
    },
    util::normalize_ident,
    vdbe::{
        affinity::Affinity,
        builder::{CursorType, ProgramBuilder},
        insn::{to_u32, AggStepData, CmpInsFlags, Cookie, Insn, RegisterOrLiteral},
    },
    Result,
};
use amphora_engine_parser::ast;

/// A table paired with an optional specific index to analyze.
type AnalyzeTarget = (Arc<BTreeTable>, Option<Arc<Index>>);

/// Resolve the target database_id and collect analyze targets from the QualifiedName.
///
/// ANALYZE can target:
/// - Nothing (None): analyze all tables in main (database_id = 0)
/// - A database name ("main", "aux"): analyze all tables in that database
/// - A table name: analyze all indexes on that table
/// - A qualified table name (db.table): analyze in the specified database
/// - An index name: analyze just that index
fn resolve_analyze_targets(
    target_opt: &Option<ast::QualifiedName>,
    resolver: &Resolver,
) -> Result<(usize, Vec<AnalyzeTarget>)> {
    match target_opt {
        Some(target) => {
            let normalized = normalize_ident(target.name.as_str());

            // If db_name is specified, resolve to that database
            if let Some(db_name) = &target.db_name {
                let database_id = resolver.resolve_database_id(target)?;
                let db_normalized = normalize_ident(db_name.as_str());

                // "ANALYZE db.table" — the name part is the table/index
                // But first check if the name is actually a database name too (shouldn't be with db_name set)
                let targets = resolve_targets_in_db(&normalized, database_id, resolver)?;
                if targets.is_empty() {
                    bail_parse_error!("no such table or index: {}.{}", db_normalized, normalized);
                }
                return Ok((database_id, targets));
            }

            // No db_name — check if the name is a database name first
            if normalized.eq_ignore_ascii_case("main") {
                let targets = collect_all_tables_in_db(0, resolver);
                return Ok((0, targets));
            }

            // Check if it's an attached database name
            if let Some((db_id, _)) = resolver.get_attached_database(&normalized) {
                let targets = collect_all_tables_in_db(db_id, resolver);
                return Ok((db_id, targets));
            }

            // Not a database name — search main schema for table/index
            let targets = resolve_targets_in_db(&normalized, 0, resolver)?;
            if targets.is_empty() {
                bail_parse_error!("no such table or index: {}", target.name);
            }
            Ok((0, targets))
        }
        None => {
            // ANALYZE with no target — analyze all tables in main
            let targets = collect_all_tables_in_db(0, resolver);
            Ok((0, targets))
        }
    }
}

/// Collect all user tables in the given database.
fn collect_all_tables_in_db(database_id: usize, resolver: &Resolver) -> Vec<AnalyzeTarget> {
    resolver.with_schema(database_id, |schema| {
        schema
            .tables
            .iter()
            .filter_map(|(name, table)| {
                if RESERVED_TABLE_PREFIXES
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
                {
                    return None;
                }
                table.btree().map(|bt| (bt, None))
            })
            .collect()
    })
}

/// Resolve a name as a table or index within a specific database.
fn resolve_targets_in_db(
    name: &str,
    database_id: usize,
    resolver: &Resolver,
) -> Result<Vec<AnalyzeTarget>> {
    // Try as a table first
    let table_opt: Option<Arc<BTreeTable>> =
        resolver.with_schema(database_id, |s| s.get_btree_table(name));
    if let Some(table) = table_opt {
        return Ok(vec![(table, None)]);
    }

    // Try as an index
    let found: Option<(Arc<BTreeTable>, Arc<Index>)> =
        resolver.with_schema(database_id, |schema| {
            for (table_name, indexes) in schema.indexes.iter() {
                if let Some(index) = indexes
                    .iter()
                    .find(|idx| idx.is_public() && idx.name.eq_ignore_ascii_case(name))
                {
                    if let Some(table) = schema.get_btree_table(table_name) {
                        return Some((table, index.clone()));
                    }
                }
            }
            None
        });
    if let Some((table, index)) = found {
        return Ok(vec![(table, Some(index))]);
    }

    Ok(vec![])
}

pub fn translate_analyze(
    target_opt: Option<ast::QualifiedName>,
    resolver: &Resolver,
    program: &mut ProgramBuilder,
) -> Result<()> {
    // Resolve the target database and collect analyze targets.
    let (database_id, analyze_targets) = resolve_analyze_targets(&target_opt, resolver)?;

    if analyze_targets.is_empty() {
        return Ok(());
    }

    // Register a write transaction for the target database so that the
    // epilogue emits a Transaction instruction (which starts the MVCC
    // exclusive transaction required by OpenWrite on sqlite_schema).
    let schema_cookie = resolver.with_schema(database_id, |s| s.schema_version);
    program.begin_write_on_database(database_id, schema_cookie)?;
    program.begin_write_operation()?;

    // This is emitted early because SQLite does, and thus generated VDBE matches a bit closer.
    let null_reg = program.alloc_register();
    program.emit_insn(Insn::Null {
        dest: null_reg,
        dest_end: None,
    });

    let stat_cursor = open_stats_table(
        program,
        resolver,
        database_id,
        STAT1_TABLE,
        "CREATE TABLE sqlite_stat1(tbl,idx,stat)",
    )?;
    let column_stats_cursor = open_stats_table(
        program,
        resolver,
        database_id,
        COLUMN_STATS_TABLE,
        COLUMN_STATS_SQL,
    )?;

    for (target_table, target_index) in analyze_targets {
        // Remove existing stat rows for this target before inserting fresh ones.
        let rewind_done = program.allocate_label();
        program.emit_insn(Insn::Rewind {
            cursor_id: stat_cursor,
            pc_if_empty: rewind_done,
        });
        let loop_start = program.allocate_label();
        program.preassign_label_to_next_insn(loop_start);

        let tbl_col_reg = program.alloc_register();
        program.emit_insn(Insn::Column {
            cursor_id: stat_cursor,
            column: 0,
            dest: tbl_col_reg,
            default: None,
        });
        let target_tbl_reg = program.alloc_register();
        program.emit_insn(Insn::String8 {
            value: target_table.name.to_string(),
            dest: target_tbl_reg,
        });
        program.mark_last_insn_constant();

        let skip_label = program.allocate_label();
        program.emit_insn(Insn::Ne {
            lhs: tbl_col_reg,
            rhs: target_tbl_reg,
            target_pc: skip_label,
            flags: Default::default(),
            collation: None,
        });

        if let Some(idx) = target_index.clone() {
            let idx_col_reg = program.alloc_register();
            program.emit_insn(Insn::Column {
                cursor_id: stat_cursor,
                column: 1,
                dest: idx_col_reg,
                default: None,
            });
            let target_idx_reg = program.alloc_register();
            program.emit_insn(Insn::String8 {
                value: idx.name.to_string(),
                dest: target_idx_reg,
            });
            program.mark_last_insn_constant();
            program.emit_insn(Insn::Ne {
                lhs: idx_col_reg,
                rhs: target_idx_reg,
                target_pc: skip_label,
                flags: Default::default(),
                collation: None,
            });
            let rowid_reg = program.alloc_register();
            program.emit_insn(Insn::RowId {
                cursor_id: stat_cursor,
                dest: rowid_reg,
            });
            program.emit_insn(Insn::Delete {
                cursor_id: stat_cursor,
                table_name: "sqlite_stat1".to_string(),
                is_part_of_update: false,
            });
            program.emit_insn(Insn::Next {
                cursor_id: stat_cursor,
                pc_if_next: loop_start,
                fullscan: false,
                is_index: false,
            });
        } else {
            let rowid_reg = program.alloc_register();
            program.emit_insn(Insn::RowId {
                cursor_id: stat_cursor,
                dest: rowid_reg,
            });
            program.emit_insn(Insn::Delete {
                cursor_id: stat_cursor,
                table_name: "sqlite_stat1".to_string(),
                is_part_of_update: false,
            });
            program.emit_insn(Insn::Next {
                cursor_id: stat_cursor,
                pc_if_next: loop_start,
                fullscan: false,
                is_index: false,
            });
        }

        program.preassign_label_to_next_insn(skip_label);
        program.emit_insn(Insn::Next {
            cursor_id: stat_cursor,
            pc_if_next: loop_start,
            fullscan: false,
            is_index: false,
        });
        program.preassign_label_to_next_insn(rewind_done);

        let tablename_reg = program.alloc_register();
        program.emit_insn(Insn::String8 {
            value: target_table.name.to_string(),
            dest: tablename_reg,
        });
        program.mark_last_insn_constant();
        // Emit index stats for this table (or for a single index target).
        let is_specific_index_target = target_index.is_some();
        let indexes: Vec<Arc<Index>> = match target_index {
            Some(idx) => vec![idx],
            None => {
                // Amphora: a WITHOUT ROWID table's primary key is its B-tree, which the schema's
                // indexes leave out (docs/design/planner.md decision 15). SQLite counts it as an
                // index named after its table, as the key's descriptor is (bug 5adc6889).
                let primary = if target_table.has_rowid {
                    None
                } else {
                    Some(Arc::new(Index::without_rowid_primary_key(&target_table)?))
                };
                resolver.with_schema(database_id, |s| {
                    primary
                        .into_iter()
                        .chain(
                            s.get_indices(&target_table.name)
                                // Amphora: an index an online schema change hasn't made public lacks
                                // entries, so its statistics would mislead the plans made after (CP-T1.12).
                                .filter(|idx| idx.index_method.is_none() && idx.is_public()) // skip custom for now
                                .cloned(),
                        )
                        .collect()
                })
            }
        };
        // Match SQLite: emit the table-level sqlite_stat1 row only when ANALYZE
        // targets a table and there are no non-partial indexes contributing stats.
        let emit_table_stat =
            !is_specific_index_target && !indexes.iter().any(|index| index.where_clause.is_none());
        if emit_table_stat {
            let target_cursor =
                program.alloc_cursor_id(CursorType::BTreeTable(target_table.clone()));
            program.emit_insn(Insn::OpenRead {
                cursor_id: target_cursor,
                root_page: target_table.root_page,
                db: database_id,
            });
            let indexname_reg = program.alloc_register();
            let stat_text_reg = program.alloc_register();
            let record_reg = program.alloc_register();
            let count_reg = program.alloc_register();
            program.emit_insn(Insn::Count {
                cursor_id: target_cursor,
                target_reg: count_reg,
                exact: true,
            });
            let after_insert = program.allocate_label();
            program.emit_insn(Insn::IfNot {
                reg: count_reg,
                target_pc: after_insert,
                jump_if_null: false,
            });
            program.emit_insn(Insn::Null {
                dest: indexname_reg,
                dest_end: None,
            });
            // stat = CAST(count AS TEXT)
            program.emit_insn(Insn::Copy {
                src_reg: count_reg,
                dst_reg: stat_text_reg,
                extra_amount: 0,
            });
            program.emit_insn(Insn::Cast {
                reg: stat_text_reg,
                affinity: Affinity::Text,
            });
            program.emit_insn(Insn::MakeRecord {
                start_reg: to_u32(tablename_reg),
                count: to_u32(3),
                dest_reg: to_u32(record_reg),
                index_name: None,
                affinity_str: None,
            });
            let rowid_reg = program.alloc_register();
            program.emit_insn(Insn::NewRowid {
                cursor: stat_cursor,
                rowid_reg,
                prev_largest_reg: 0,
            });
            // FIXME: SQLite sets OPFLAG_APPEND on the insert, but that's not supported in turso right now.
            // SQLite doesn't emit the table name, but like... why not?
            program.emit_insn(Insn::Insert {
                cursor: stat_cursor,
                key_reg: rowid_reg,
                record_reg,
                flag: Default::default(),
                table_name: "sqlite_stat1".to_string(),
            });
            program.preassign_label_to_next_insn(after_insert);
        }
        // The column statistics first: their pass deletes the table's old rows, the index
        // degrees' (PL.11) among them, before the index passes below keep the new ones. ANALYZE
        // of one index keeps the table's column statistics as they were, as it keeps its
        // other indexes' `sqlite_stat1` rows.
        if !is_specific_index_target {
            emit_column_stats(program, column_stats_cursor, &target_table, database_id)?;
            let cursor = program.alloc_cursor_id(CursorType::BTreeTable(target_table.clone()));
            emit_tree_pages(
                program,
                column_stats_cursor,
                cursor,
                target_table.root_page,
                &target_table.name,
                None,
                database_id,
            )?;
        }
        let degrees_cursor = if is_specific_index_target {
            None
        } else {
            Some(column_stats_cursor)
        };
        for index in indexes {
            emit_index_stats(
                program,
                stat_cursor,
                degrees_cursor,
                &target_table,
                &index,
                database_id,
            )?;
        }
    }

    // FIXME: Emit LoadAnalysis
    Ok(())
}

/// The table ANALYZE keeps its sketches and samples in (docs/design/planner.md, decision 20): an
/// internal table of the engine's reserved prefix, which SQLite and earlier builds read as any
/// other table and never consult.
pub(crate) const COLUMN_STATS_TABLE: &str = "__turso_internal_column_stats";
const COLUMN_STATS_SQL: &str = "CREATE TABLE __turso_internal_column_stats(tbl,col,kind,data)";
const STAT1_TABLE: &str = "sqlite_stat1";

/// The register `at` places past `start`, of a block `alloc_registers` gave.
fn register(start: usize, at: usize) -> Result<usize> {
    start
        .checked_add(at)
        .ok_or_else(|| crate::LimboError::InternalError("ANALYZE ran out of registers".into()))
}

/// Opens the statistics table `name` for writing, creating it by `sql` first where the schema
/// has none, as SQLite creates `sqlite_stat1`: a B-tree, its schema row, the schema parsed again
/// and its cookie moved on. Returns its cursor.
fn open_stats_table(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    database_id: usize,
    name: &str,
    sql: &str,
) -> Result<usize> {
    let existing: Option<Arc<BTreeTable>> =
        resolver.with_schema(database_id, |s| s.get_btree_table(name));
    let (table, root) = if let Some(table) = existing {
        let root = RegisterOrLiteral::Literal(table.root_page);
        (table, root)
    } else {
        let table_root_reg = program.alloc_register();
        program.emit_insn(Insn::CreateBtree {
            db: database_id,
            root: table_root_reg,
            flags: CreateBTreeFlags::new_table(),
        });
        // The root page 0 is never read: the cursor opens the register's root.
        let table = Arc::new(BTreeTable::from_sql(sql, 0)?);
        let schema_table = resolver
            .with_schema(database_id, |s| s.get_btree_table(SQLITE_TABLEID))
            .ok_or_else(|| crate::LimboError::InternalError("no sqlite_schema".into()))?;
        let sqlite_schema_cursor_id = program.alloc_cursor_id(CursorType::BTreeTable(schema_table));
        program.emit_insn(Insn::OpenWrite {
            cursor_id: sqlite_schema_cursor_id,
            root_page: 1i64.into(),
            db: database_id,
        });
        emit_schema_entry(
            program,
            resolver,
            sqlite_schema_cursor_id,
            None,
            SchemaEntryType::Table,
            name,
            name,
            table_root_reg,
            Some(sql.to_string()),
        )?;
        program.emit_insn(Insn::ParseSchema {
            db: database_id,
            where_clause: Some(format!("tbl_name = '{name}' AND type != 'trigger'")),
            trigger_target_database_id: None,
        });
        let schema_version = resolver.with_schema(database_id, |s| s.schema_version);
        let next = i32::try_from(schema_version)
            .ok()
            .and_then(|v| v.checked_add(1))
            .ok_or_else(|| crate::LimboError::InternalError("schema cookie overflow".into()))?;
        program.emit_insn(Insn::SetCookie {
            db: database_id,
            cookie: Cookie::SchemaVersion,
            value: next,
            p5: 0,
        });
        (table, RegisterOrLiteral::Register(table_root_reg))
    };
    let cursor = program.alloc_cursor_id(CursorType::BTreeTable(table));
    program.emit_insn(Insn::OpenWrite {
        cursor_id: cursor,
        root_page: root,
        db: database_id,
    });
    Ok(cursor)
}

/// Emits one scan of `table` that sketches each of its columns and samples its rows (decision
/// 20), after deleting the rows the statistics table holds for it; then a row a column, `(tbl,
/// col, 'sketch', the sketch's encoding)`, and one `(tbl, NULL, 'sample', the sample's encoding)`,
/// its data NULL where the table's rows passed the sample's byte budget. The scan's loop is the
/// table's rows; its body is bounded by the table's columns.
fn emit_column_stats(
    program: &mut ProgramBuilder,
    stats_cursor: usize,
    table: &Arc<BTreeTable>,
    database_id: usize,
) -> Result<()> {
    emit_delete_rows_of(program, stats_cursor, &table.name);
    let columns: Vec<(usize, String)> = table
        .columns()
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.is_generated())
        .filter_map(|(i, c)| c.name.clone().map(|name| (i, name)))
        .collect();
    let count = columns.len();
    if count == 0 {
        return Ok(());
    }
    let sketch = AggFunc::External(Arc::new(ExtFunc::NativeAggregate {
        argc: 1,
        function: Arc::new(SketchFunction),
    }));
    let sample = AggFunc::External(Arc::new(ExtFunc::NativeAggregate {
        argc: 1,
        function: Arc::new(SampleFunction {
            table: table.name.clone(),
        }),
    }));
    let accumulators = program.alloc_registers(count);
    let sample_acc = program.alloc_register();
    let values = program.alloc_registers(count);
    let record = program.alloc_register();
    for at in 0..count {
        program.emit_insn(Insn::Null {
            dest: register(accumulators, at)?,
            dest_end: None,
        });
    }
    program.emit_insn(Insn::Null {
        dest: sample_acc,
        dest_end: None,
    });
    let cursor = program.alloc_cursor_id(CursorType::BTreeTable(table.clone()));
    program.emit_insn(Insn::OpenRead {
        cursor_id: cursor,
        root_page: table.root_page,
        db: database_id,
    });
    let done = program.allocate_label();
    let next_row = program.allocate_label();
    program.emit_insn(Insn::Rewind {
        cursor_id: cursor,
        pc_if_empty: done,
    });
    program.preassign_label_to_next_insn(next_row);
    for (at, (column, _)) in columns.iter().enumerate() {
        let value = register(values, at)?;
        program.emit_column_or_rowid(cursor, *column, value);
        program.emit_insn(Insn::AggStep {
            data: Box::new(AggStepData {
                acc_reg: register(accumulators, at)?,
                col: value,
                delimiter: 0,
                func: AccumulatorFunc::Agg(sketch.clone()),
                comparator: None,
                collation: None,
            }),
        });
    }
    program.emit_insn(Insn::MakeRecord {
        start_reg: to_u32(values),
        count: to_u32(count),
        dest_reg: to_u32(record),
        index_name: None,
        affinity_str: None,
    });
    program.emit_insn(Insn::AggStep {
        data: Box::new(AggStepData {
            acc_reg: sample_acc,
            col: record,
            delimiter: 0,
            func: AccumulatorFunc::Agg(sample.clone()),
            comparator: None,
            collation: None,
        }),
    });
    program.emit_insn(Insn::Next {
        cursor_id: cursor,
        pc_if_next: next_row,
        fullscan: true,
        is_index: false,
    });
    program.preassign_label_to_next_insn(done);
    let row = program.alloc_registers(4);
    let (tbl, col, kind, data) = (row, register(row, 1)?, register(row, 2)?, register(row, 3)?);
    program.emit_insn(Insn::String8 {
        value: table.name.to_string(),
        dest: tbl,
    });
    let insert = |program: &mut ProgramBuilder| {
        let record = program.alloc_register();
        program.emit_insn(Insn::MakeRecord {
            start_reg: to_u32(row),
            count: to_u32(4),
            dest_reg: to_u32(record),
            index_name: None,
            affinity_str: None,
        });
        let rowid = program.alloc_register();
        program.emit_insn(Insn::NewRowid {
            cursor: stats_cursor,
            rowid_reg: rowid,
            prev_largest_reg: 0,
        });
        program.emit_insn(Insn::Insert {
            cursor: stats_cursor,
            key_reg: rowid,
            record_reg: record,
            flag: Default::default(),
            table_name: COLUMN_STATS_TABLE.to_string(),
        });
    };
    for (at, (_, name)) in columns.iter().enumerate() {
        let acc = register(accumulators, at)?;
        program.emit_insn(Insn::AggFinal {
            register: acc,
            func: AccumulatorFunc::Agg(sketch.clone()),
        });
        program.emit_insn(Insn::String8 {
            value: name.clone(),
            dest: col,
        });
        program.emit_insn(Insn::String8 {
            value: "sketch".to_string(),
            dest: kind,
        });
        program.emit_insn(Insn::Copy {
            src_reg: acc,
            dst_reg: data,
            extra_amount: 0,
        });
        insert(program);
    }
    program.emit_insn(Insn::AggFinal {
        register: sample_acc,
        func: AccumulatorFunc::Agg(sample),
    });
    program.emit_insn(Insn::Null {
        dest: col,
        dest_end: None,
    });
    program.emit_insn(Insn::String8 {
        value: "sample".to_string(),
        dest: kind,
    });
    program.emit_insn(Insn::Copy {
        src_reg: sample_acc,
        dst_reg: data,
        extra_amount: 0,
    });
    insert(program);
    Ok(())
}

/// Amphora (PL.7, T-PL.7.1): emits a walk of the B-tree at `root`, opened at `tree`, that counts
/// its pages ([Insn::TreePages]), and keeps them at `stats_cursor`, `(tbl, index or NULL, 'pages',
/// the count)`: the table's tree where `index` is `None`. The planner reads a tree's rows a page
/// from them (`cost::TreeShape`). The walk reads each interior and leaf page once and no record.
fn emit_tree_pages(
    program: &mut ProgramBuilder,
    stats_cursor: usize,
    tree: usize,
    root: i64,
    table: &str,
    index: Option<&str>,
    database_id: usize,
) -> Result<()> {
    program.emit_insn(Insn::OpenRead {
        cursor_id: tree,
        root_page: root,
        db: database_id,
    });
    let row = program.alloc_registers(4);
    let (tbl, idx, kind, data) = (row, register(row, 1)?, register(row, 2)?, register(row, 3)?);
    program.emit_insn(Insn::String8 {
        value: table.to_string(),
        dest: tbl,
    });
    match index {
        Some(index) => program.emit_insn(Insn::String8 {
            value: index.to_string(),
            dest: idx,
        }),
        None => program.emit_insn(Insn::Null {
            dest: idx,
            dest_end: None,
        }),
    }
    program.emit_insn(Insn::String8 {
        value: "pages".to_string(),
        dest: kind,
    });
    program.emit_insn(Insn::TreePages {
        cursor_id: tree,
        target_reg: data,
    });
    let record = program.alloc_register();
    program.emit_insn(Insn::MakeRecord {
        start_reg: to_u32(row),
        count: to_u32(4),
        dest_reg: to_u32(record),
        index_name: None,
        affinity_str: None,
    });
    let rowid = program.alloc_register();
    program.emit_insn(Insn::NewRowid {
        cursor: stats_cursor,
        rowid_reg: rowid,
        prev_largest_reg: 0,
    });
    program.emit_insn(Insn::Insert {
        cursor: stats_cursor,
        key_reg: rowid,
        record_reg: record,
        flag: Default::default(),
        table_name: COLUMN_STATS_TABLE.to_string(),
    });
    Ok(())
}

/// Emits a pass over the statistics table at `cursor` that deletes each row whose first column
/// is `table`.
fn emit_delete_rows_of(program: &mut ProgramBuilder, cursor: usize, table: &str) {
    let done = program.allocate_label();
    let top = program.allocate_label();
    let skip = program.allocate_label();
    program.emit_insn(Insn::Rewind {
        cursor_id: cursor,
        pc_if_empty: done,
    });
    program.preassign_label_to_next_insn(top);
    let tbl = program.alloc_register();
    program.emit_insn(Insn::Column {
        cursor_id: cursor,
        column: 0,
        dest: tbl,
        default: None,
    });
    let name = program.alloc_register();
    program.emit_insn(Insn::String8 {
        value: table.to_string(),
        dest: name,
    });
    program.emit_insn(Insn::Ne {
        lhs: tbl,
        rhs: name,
        target_pc: skip,
        flags: Default::default(),
        collation: None,
    });
    program.emit_insn(Insn::Delete {
        cursor_id: cursor,
        table_name: COLUMN_STATS_TABLE.to_string(),
        is_part_of_update: false,
    });
    program.emit_insn(Insn::Next {
        cursor_id: cursor,
        pc_if_next: top,
        fullscan: false,
        is_index: false,
    });
    program.preassign_label_to_next_insn(skip);
    program.emit_insn(Insn::Next {
        cursor_id: cursor,
        pc_if_next: top,
        fullscan: false,
        is_index: false,
    });
    program.preassign_label_to_next_insn(done);
}

/// Emit VDBE code to gather and insert statistics for a single index.
///
/// This uses the stat_init/stat_push/stat_get functions to collect statistics.
/// The bytecode scans the index in sorted order, comparing columns to detect
/// when prefixes change, and calls stat_push with the change index.
///
/// The stat string format is: "total avg1 avg2 avg3"
/// where avgN = ceil(total / distinctN) = average rows per distinct prefix
///
/// Amphora (PL.11): with `degrees_cursor`, an index without a WHERE (all the table's rows) also
/// keeps each prefix's degree norms there, `(tbl, index, 'degrees', stat_degrees(accum))`.
fn emit_index_stats(
    program: &mut ProgramBuilder,
    stat_cursor: usize,
    degrees_cursor: Option<usize>,
    table: &Arc<BTreeTable>,
    index: &Arc<Index>,
    database_id: usize,
) -> Result<()> {
    let n_cols = index.columns.len();
    if n_cols == 0 {
        return Ok(());
    }

    // Open the index cursor
    let idx_cursor = program.alloc_cursor_id(CursorType::BTreeIndex(index.clone()));
    program.emit_insn(Insn::OpenRead {
        cursor_id: idx_cursor,
        root_page: index.root_page,
        db: database_id,
    });

    // Allocate registers contiguously for stat_push(accum, chng):
    let reg_accum = program.alloc_register();
    let reg_chng = program.alloc_register();

    // Registers for previous row values and comparison temp
    let reg_prev_base = program.alloc_registers(n_cols);
    let reg_temp = program.alloc_register();

    // Initialize the accumulator with stat_init(n_cols)
    // Reuse reg_chng temporarily for the n_cols argument
    program.emit_insn(Insn::Integer {
        value: n_cols as i64,
        dest: reg_chng,
    });
    program.emit_insn(Insn::Function {
        constant_mask: 0,
        start_reg: reg_chng,
        dest: reg_accum,
        func: FuncCtx {
            func: Func::Scalar(ScalarFunc::StatInit),
            arg_count: 1,
            collation: None,
        },
    });

    // Labels for control flow
    let lbl_empty = program.allocate_label();
    let lbl_loop = program.allocate_label();
    let lbl_stat_push = program.allocate_label();

    // We need one label per column for the update_prev jump targets
    let lbl_update_prev: Vec<_> = (0..n_cols).map(|_| program.allocate_label()).collect();

    // Rewind the index cursor; if empty, skip to end
    program.emit_insn(Insn::Rewind {
        cursor_id: idx_cursor,
        pc_if_empty: lbl_empty,
    });

    // First row: set chng=0 and jump to update all prev columns
    program.emit_insn(Insn::Integer {
        value: 0,
        dest: reg_chng,
    });
    program.emit_insn(Insn::Goto {
        target_pc: lbl_update_prev[0],
    });

    // Main loop: compare columns to find change point
    program.preassign_label_to_next_insn(lbl_loop);

    // Set reg_chng = 0, then check each column
    program.emit_insn(Insn::Integer {
        value: 0,
        dest: reg_chng,
    });

    for (i, lbl) in lbl_update_prev.iter().enumerate().take(n_cols) {
        program.emit_insn(Insn::Column {
            cursor_id: idx_cursor,
            column: i,
            dest: reg_temp,
            default: None,
        });
        program.emit_insn(Insn::Ne {
            lhs: reg_temp,
            rhs: reg_prev_base + i,
            target_pc: *lbl,
            flags: CmpInsFlags::default().null_eq(),
            collation: index.columns[i].collation,
        });
        // If columns match, increment chng and continue to next column
        if i < n_cols - 1 {
            program.emit_insn(Insn::Integer {
                value: (i + 1) as i64,
                dest: reg_chng,
            });
        }
    }

    // All columns equal - chng = n_cols (duplicate row), jump over update section to stat_push
    program.emit_insn(Insn::Integer {
        value: n_cols as i64,
        dest: reg_chng,
    });
    program.emit_insn(Insn::Goto {
        target_pc: lbl_stat_push,
    });

    // Update prev section: emit n_cols consecutive Column instructions that cascade
    // When col i differs from prev, jump here to update prev[i], prev[i+1], ..., prev[n_cols-1]
    for (i, lbl) in lbl_update_prev.iter().enumerate().take(n_cols) {
        program.preassign_label_to_next_insn(*lbl);
        program.emit_insn(Insn::Column {
            cursor_id: idx_cursor,
            column: i,
            dest: reg_prev_base + i,
            default: None,
        });
        // Fall through to next column update, then to stat_push
    }

    program.preassign_label_to_next_insn(lbl_stat_push);
    program.emit_insn(Insn::Function {
        constant_mask: 0,
        start_reg: reg_accum,
        dest: reg_accum,
        func: FuncCtx {
            func: Func::Scalar(ScalarFunc::StatPush),
            arg_count: 2,
            collation: None,
        },
    });

    // Next iteration
    program.emit_insn(Insn::Next {
        cursor_id: idx_cursor,
        pc_if_next: lbl_loop,
        fullscan: false,
        is_index: false,
    });

    // stat_get(accum) to get the final stat string
    let reg_stat = program.alloc_register();
    program.emit_insn(Insn::Function {
        constant_mask: 0,
        start_reg: reg_accum,
        dest: reg_stat,
        func: FuncCtx {
            func: Func::Scalar(ScalarFunc::StatGet),
            arg_count: 1,
            collation: None,
        },
    });

    // Skip insert if stat is NULL (empty index)
    program.emit_insn(Insn::IsNull {
        reg: reg_stat,
        target_pc: lbl_empty,
    });

    // Insert record into sqlite_stat1
    // Allocate contiguous registers for MakeRecord: tablename, indexname, stat
    let record_start = program.alloc_registers(3);
    program.emit_insn(Insn::String8 {
        value: table.name.to_string(),
        dest: record_start,
    });
    program.mark_last_insn_constant();
    program.emit_insn(Insn::String8 {
        value: index.name.to_string(),
        dest: record_start + 1,
    });
    program.mark_last_insn_constant();
    program.emit_insn(Insn::Copy {
        src_reg: reg_stat,
        dst_reg: record_start + 2,
        extra_amount: 0,
    });

    let idx_record_reg = program.alloc_register();
    program.emit_insn(Insn::MakeRecord {
        start_reg: to_u32(record_start),
        count: to_u32(3),
        dest_reg: to_u32(idx_record_reg),
        index_name: None,
        affinity_str: None,
    });

    let idx_rowid_reg = program.alloc_register();
    program.emit_insn(Insn::NewRowid {
        cursor: stat_cursor,
        rowid_reg: idx_rowid_reg,
        prev_largest_reg: 0,
    });
    program.emit_insn(Insn::Insert {
        cursor: stat_cursor,
        key_reg: idx_rowid_reg,
        record_reg: idx_record_reg,
        flag: Default::default(),
        table_name: "sqlite_stat1".to_string(),
    });

    if let Some(cursor) = degrees_cursor {
        let tree = program.alloc_cursor_id(CursorType::BTreeIndex(index.clone()));
        emit_tree_pages(
            program,
            cursor,
            tree,
            index.root_page,
            &table.name,
            Some(&index.name),
            database_id,
        )?;
    }

    if let Some(cursor) = degrees_cursor.filter(|_| index.where_clause.is_none()) {
        let row = program.alloc_registers(4);
        let (tbl, idx, kind, data) = (row, register(row, 1)?, register(row, 2)?, register(row, 3)?);
        program.emit_insn(Insn::String8 {
            value: table.name.to_string(),
            dest: tbl,
        });
        program.emit_insn(Insn::String8 {
            value: index.name.to_string(),
            dest: idx,
        });
        program.emit_insn(Insn::String8 {
            value: "degrees".to_string(),
            dest: kind,
        });
        program.emit_insn(Insn::Function {
            constant_mask: 0,
            start_reg: reg_accum,
            dest: data,
            func: FuncCtx {
                func: Func::Scalar(ScalarFunc::StatDegrees),
                arg_count: 1,
                collation: None,
            },
        });
        let record = program.alloc_register();
        program.emit_insn(Insn::MakeRecord {
            start_reg: to_u32(row),
            count: to_u32(4),
            dest_reg: to_u32(record),
            index_name: None,
            affinity_str: None,
        });
        let rowid = program.alloc_register();
        program.emit_insn(Insn::NewRowid {
            cursor,
            rowid_reg: rowid,
            prev_largest_reg: 0,
        });
        program.emit_insn(Insn::Insert {
            cursor,
            key_reg: rowid,
            record_reg: record,
            flag: Default::default(),
            table_name: COLUMN_STATS_TABLE.to_string(),
        });
    }

    // Label for empty index case, just skip the insert
    program.preassign_label_to_next_insn(lbl_empty);
    Ok(())
}
