//! Online schema changes by F1's intermediate states (CP-T1.12, T-T1.12.1; docs/design/online-schema.md).
//!
//! An index is built without holding the writer for its table's size: it goes absent, delete-only,
//! write-only, then public (Rae et al., "Online, Asynchronous Schema Change in F1", PVLDB 6(11),
//! 2013, section 4), each step one transaction through the writer. While write-only, writes keep it
//! and batches of a backfill give the rows written before it their entries; the cutover to public
//! changes only the catalog. Every step is recorded in `__amphora_schema_changes` in the same
//! transaction as its effect, so a crash leaves the change at a step, invisible to reads until it is
//! public, and the next call carries it on from there.
//!
//! A column is added the same way with its states run together: `ALTER TABLE … ADD COLUMN` rewrites
//! no row (each row written before it reads the column's constant default), so absent to public is
//! one catalog change, its record in the same transaction. A column whose addition would read every
//! row (a generated one with NOT NULL or CHECK, which the engine checks against each row) is
//! refused.
//!
//! A constraint (NOT NULL or CHECK) is added to an existing column by `ALTER TABLE … ALTER COLUMN c
//! TO <its definition plus the constraint>` through write-only, validated, public: one transaction
//! marks the column unvalidated in `__amphora_constraint_states` and redefines it, so writes are
//! checked from then on while the planner doesn't rely on it; batches check the rows written before
//! it in rowid order; the cutover removes the mark, a catalog change only. A row that breaks it
//! undoes the change: the column's old definition back.
//!
//! A column is dropped by `ALTER TABLE … DROP COLUMN` PostgreSQL's way: one catalog change names it
//! in `__amphora_dropped_columns`, a slot the engine hides from names, `*`, inserts and `PRAGMA
//! table_info` and every write stores NULL in; batches rewrite the rows before it in rowid order,
//! clearing their values; then the drop is recorded.

use super::*;
use amphora_engine::schema::IndexState;
use amphora_engine_parser::ast::{
    AlterTableBody, ColumnConstraint, ColumnDefinition, CreateTableBody,
};
use std::time::Duration;

/// The tables a change keeps, made by its first step (never at open: a replica on a binary from
/// before them would not make them, as `application.rs` says of its own).
const TABLES: &str = "
CREATE TABLE IF NOT EXISTS __amphora_index_states (name TEXT PRIMARY KEY, state INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS __amphora_schema_changes (name TEXT PRIMARY KEY, sql TEXT NOT NULL, idx TEXT NOT NULL, tbl TEXT NOT NULL, phase INTEGER NOT NULL, after INTEGER, until INTEGER);
CREATE TABLE IF NOT EXISTS __amphora_constraint_states (tbl TEXT NOT NULL, col TEXT NOT NULL, old TEXT NOT NULL, PRIMARY KEY (tbl, col));
CREATE TABLE IF NOT EXISTS __amphora_dropped_columns (tbl TEXT NOT NULL, col TEXT NOT NULL, PRIMARY KEY (tbl, col));";

/// The phases a change records (a format value of `__amphora_schema_changes.phase`): its index
/// delete-only, write-only with its backfill under way, and backfilled. Public is no row.
const DELETE_ONLY: i64 = 1;
const WRITE_ONLY: i64 = 2;
const BACKFILLED: i64 = 3;

/// What a refusal says when the table's rows break the unique index or the constraint a change
/// would add: the change can't finish, and its driver undoes it.
pub const BROKEN: &str = "online schema change's rows";

/// A change asked for: its migration's name and SQL (one `CREATE [UNIQUE] INDEX`, `ALTER TABLE …
/// ADD COLUMN` or `ALTER TABLE … ALTER COLUMN` adding a constraint), what it adds, and its table as SQL names it (case-folded, as the engine's
/// `normalize_ident` folds names).
pub struct Plan {
    name: String,
    sql: String,
    adds: Adds,
    table: String,
}

/// What a change adds.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Adds {
    /// An index, by its case-folded name: built absent, delete-only, write-only, public.
    Index(String),
    /// A column whose addition rewrites and reads no row: absent to public in one catalog change.
    Column,
    /// NOT NULL or CHECK on an existing column, by its case-folded name and new definition:
    /// write-only, validated in batches, public.
    Constraint {
        column: String,
        definition: ColumnDefinition,
    },
    /// A column dropped, by its case-folded name: hidden in one catalog change (a slot every
    /// write stores NULL in), its old values cleared in batches, then recorded.
    DropColumn(String),
}

impl Plan {
    pub fn parse(migration: &Migration) -> Result<Self, Refusal> {
        let refuse = |why: &str| api::invalid("online schema change", why);
        let mut parser = Parser::new(migration.sql.as_bytes());
        let first = parser
            .next_cmd()
            .map_err(|e| refuse(&e.to_string()))?
            .ok_or_else(|| refuse("no statement"))?;
        if parser
            .next_cmd()
            .map_err(|e| refuse(&e.to_string()))?
            .is_some()
        {
            return Err(refuse("one statement"));
        }
        let (adds, table) = match first {
            Cmd::Stmt(Stmt::CreateIndex {
                if_not_exists,
                idx_name,
                tbl_name,
                using,
                ..
            }) => {
                // Staging a name an index already has would turn that index's reads off: the
                // change names an index that must not exist, and applies once by its migration's
                // name.
                if if_not_exists {
                    return Err(refuse("IF NOT EXISTS: the change applies once by name"));
                }
                if idx_name.db_name.is_some() || using.is_some() {
                    return Err(refuse("an index of the main database, without USING"));
                }
                (
                    Adds::Index(idx_name.name.as_str().to_ascii_lowercase()),
                    tbl_name,
                )
            }
            Cmd::Stmt(Stmt::AlterTable(alter)) => {
                if alter.name.db_name.is_some() {
                    return Err(refuse("a column of the main database's table"));
                }
                match alter.body {
                    AlterTableBody::AddColumn(column) => {
                        if reads_every_row(&column) {
                            return Err(refuse(
                                "a generated column with NOT NULL or CHECK is checked against every row: add the column, then its constraint by ALTER COLUMN",
                            ));
                        }
                        (Adds::Column, alter.name.name)
                    }
                    AlterTableBody::AlterColumn { old, new } => {
                        let column = old.as_str().to_ascii_lowercase();
                        if new.col_name.as_str().to_ascii_lowercase() != column {
                            return Err(refuse(
                                "ALTER COLUMN online keeps the column's name: it only adds NOT NULL or CHECK",
                            ));
                        }
                        if !new.constraints.iter().any(|c| constrains(&c.constraint)) {
                            return Err(refuse(
                                "ALTER COLUMN online adds NOT NULL or CHECK, and this definition has neither",
                            ));
                        }
                        (
                            Adds::Constraint {
                                column,
                                definition: new,
                            },
                            alter.name.name,
                        )
                    }
                    AlterTableBody::DropColumn(column) => (
                        Adds::DropColumn(column.as_str().to_ascii_lowercase()),
                        alter.name.name,
                    ),
                    _ => {
                        return Err(refuse(
                            "of ALTER TABLE, ADD COLUMN, DROP COLUMN and ALTER COLUMN adding NOT NULL or CHECK are built online: RENAME checks every statement naming the table",
                        ));
                    }
                }
            }
            _ => {
                return Err(refuse(
                    "only CREATE INDEX, ALTER TABLE … ADD COLUMN, DROP COLUMN and ALTER COLUMN adding a constraint are built online",
                ));
            }
        };
        Ok(Self {
            name: migration.name.clone(),
            sql: migration.sql.clone(),
            adds,
            table: table.as_str().to_ascii_lowercase(),
        })
    }
}

/// Whether adding `column` makes the engine read every row of its table: a generated column with
/// NOT NULL or CHECK, which `ALTER TABLE` checks against each row (`emit_column_constraints_validation`
/// in the engine's `translate/alter.rs`). Every other addition's checks read at most the table's
/// first row (is it empty) and its constant default.
fn reads_every_row(column: &ColumnDefinition) -> bool {
    let has = |wanted: fn(&ColumnConstraint) -> bool| {
        column.constraints.iter().any(|c| wanted(&c.constraint))
    };
    has(|c| matches!(c, ColumnConstraint::Generated { .. }))
        && has(|c| {
            matches!(
                c,
                ColumnConstraint::NotNull {
                    nullable: false,
                    ..
                } | ColumnConstraint::Check { .. }
            )
        })
}

/// Whether a column constraint is one an online change adds: NOT NULL or CHECK.
fn constrains(constraint: &ColumnConstraint) -> bool {
    matches!(
        constraint,
        ColumnConstraint::NotNull {
            nullable: false,
            ..
        } | ColumnConstraint::Check { .. }
    )
}

/// What one step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stepped {
    /// The migration's name was applied before, with the same SQL.
    Already,
    /// The index is made, empty and delete-only.
    DeleteOnly,
    /// The index is write-only: writes keep it from here; or the constraint is, checked on writes
    /// from here.
    WriteOnly,
    /// A backfill (or a constraint's validation) batch visited `visited` rows, holding the writer `held` for them.
    Batch { visited: u64, held: Duration },
    /// The index is public: the cutover held the writer `held`.
    Public { held: Duration },
}

/// What a change did, for its caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SchemaChange {
    /// Whether this call made the change public (false: its migration was applied before).
    pub applied: bool,
    /// The backfill's (or validation's) batches and the rows they visited, in this call.
    pub batches: u64,
    pub rows: u64,
    /// How long the cutover to public held the writer (AC-T1.12.2's pause, at the writer).
    pub cutover: Option<Duration>,
}

/// How many rows the next backfill batch takes, learned from the batches run so far as the backfill
/// runs (CP-T1.12): a batch holds the writer for as long as the one before it waited for the
/// writer and its flush, so the backfill takes at most half the writer's time, whatever the load
/// and the machine. A batch of one row starts it (the least that moves a backfill); each next is
/// the last's rows scaled by its measured wait over its measured hold, at most twice the last's (a
/// run-out doubling, as decision 9's census grows), at least one row. Too many dirty pages for one
/// transaction halves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sizer {
    rows: u64,
}

impl Sizer {
    pub(crate) fn new() -> Self {
        Self { rows: 1 }
    }

    pub(crate) fn rows(&self) -> u64 {
        self.rows
    }

    /// A batch visited `visited` rows, held the writer `held` and waited `waited` besides.
    pub(crate) fn learn(&mut self, visited: u64, held: Duration, waited: Duration) {
        if visited == 0 {
            return;
        }
        let most = visited.saturating_mul(2);
        let held = held.as_nanos();
        let want = if held == 0 {
            most
        } else {
            let scaled = u128::from(visited)
                .saturating_mul(waited.as_nanos())
                .checked_div(held)
                .unwrap_or(u128::MAX);
            u64::try_from(scaled).unwrap_or(u64::MAX)
        };
        self.rows = want.clamp(1, most.max(1));
    }

    /// A batch of this size was too large for one transaction: half of it, or None at one row.
    pub(crate) fn halve(&mut self) -> Option<u64> {
        if self.rows <= 1 {
            return None;
        }
        self.rows = self.rows.checked_div(2).unwrap_or(1).max(1);
        Some(self.rows)
    }
}

/// The change's record: its SQL, phase, the last rowid its backfill reached and the largest rowid
/// its table held when the index went write-only.
struct Record {
    sql: String,
    phase: i64,
    after: Option<i64>,
    until: Option<i64>,
}

fn optional(row: &amphora_engine::Row, n: usize) -> anyhow::Result<Option<i64>> {
    match row.get::<&EValue>(n).map_err(unavailable)? {
        EValue::Null => Ok(None),
        _ => Ok(Some(row.get::<i64>(n).map_err(unavailable)?)),
    }
}

fn record(db: &Db, plan: &Plan) -> anyhow::Result<Option<Record>> {
    let mut kept = false;
    db.query(
        "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = '__amphora_schema_changes'",
        [],
        |_| {
            kept = true;
            Ok(false)
        },
    )?;
    if !kept {
        return Ok(None);
    }
    let mut found = None;
    db.query(
        "SELECT sql, phase, after, until FROM __amphora_schema_changes WHERE name = ?1",
        [text(&plan.name)],
        |row| {
            found = Some(Record {
                sql: row.get::<String>(0).map_err(unavailable)?,
                phase: row.get::<i64>(1).map_err(unavailable)?,
                after: optional(row, 2)?,
                until: optional(row, 3)?,
            });
            Ok(false)
        },
    )?;
    Ok(found)
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn stored(state: IndexState) -> anyhow::Result<EValue> {
    state
        .stored()
        .map(EValue::from_i64)
        .ok_or_else(|| anyhow::anyhow!("a public index has no state row"))
}

/// One step of `plan`'s change, in the caller's write transaction: the next its record says.
pub fn step(db: &Db, plan: &Plan, rows: u64) -> anyhow::Result<Stepped> {
    let mut applied = None;
    db.query(
        "SELECT sql FROM __amphora_migrations WHERE name = ?1",
        [text(&plan.name)],
        |row| {
            applied = Some(row.get::<String>(0).map_err(unavailable)?);
            Ok(false)
        },
    )?;
    if let Some(sql) = applied {
        if sql == plan.sql {
            return Ok(Stepped::Already);
        }
        return Err(api::invalid("migration", "the applied name has different SQL").into());
    }
    let found = record(db, plan)?;
    let index = match &plan.adds {
        Adds::Index(index) => index.as_str(),
        Adds::Column => return add_column(db, plan),
        Adds::Constraint { column, definition } => {
            return constraint_step(db, plan, column, definition, found, rows);
        }
        Adds::DropColumn(column) => return drop_step(db, plan, column, found, rows),
    };
    let Some(record) = found else {
        return begin(db, plan, index);
    };
    if record.sql != plan.sql {
        return Err(api::invalid(
            "migration",
            "a change of this name is under way with different SQL",
        )
        .into());
    }
    match record.phase {
        DELETE_ONLY => write_only(db, plan, index),
        WRITE_ONLY => backfill(db, plan, index, record.after, record.until, rows),
        BACKFILLED => cutover(db, plan, index),
        phase => Err(anyhow::anyhow!(
            "__amphora_schema_changes: {} is at phase {phase}",
            plan.name
        )),
    }
}

/// Absent to delete-only: the name staged, then the index made under it, empty.
fn begin(db: &Db, plan: &Plan, index: &str) -> anyhow::Result<Stepped> {
    db.exec(TABLES)?;
    db.query(
        "INSERT INTO __amphora_schema_changes(name, sql, idx, tbl, phase) VALUES (?1, ?2, ?3, ?4, ?5)",
        [
            text(&plan.name),
            text(&plan.sql),
            text(index),
            text(&plan.table),
            EValue::from_i64(DELETE_ONLY),
        ],
        |_| Ok(true),
    )?;
    db.query(
        "INSERT INTO __amphora_index_states(name, state) VALUES (?1, ?2)",
        [text(index), stored(IndexState::DeleteOnly)?],
        |_| Ok(true),
    )?;
    db.restage_schema()?;
    db.exec(&plan.sql)?;
    Ok(Stepped::DeleteOnly)
}

/// Delete-only to write-only. In F1 every server reaches delete-only before any goes on; here every
/// write runs on the writer's schema, which this transaction moves, so the drain is immediate
/// (docs/design/online-schema.md says when that stops holding). The rows the backfill must reach
/// are those up to the largest rowid now: a row written after gets its entry from its write.
fn write_only(db: &Db, plan: &Plan, index: &str) -> anyhow::Result<Stepped> {
    db.query(
        "UPDATE __amphora_index_states SET state = ?2 WHERE name = ?1",
        [text(index), stored(IndexState::WriteOnly)?],
        |_| Ok(true),
    )?;
    let mut until = None;
    db.query_owned(
        &format!("SELECT max(rowid) FROM {}", quoted(&plan.table)),
        [],
        |row| {
            until = optional(row, 0)?;
            Ok(false)
        },
    )?;
    db.query(
        "UPDATE __amphora_schema_changes SET phase = ?2, until = ?3 WHERE name = ?1",
        [
            text(&plan.name),
            EValue::from_i64(WRITE_ONLY),
            until.map_or(EValue::Null, EValue::from_i64),
        ],
        |_| Ok(true),
    )?;
    db.restage_schema()?;
    Ok(Stepped::WriteOnly)
}

/// One backfill batch of at most `rows` rows after `after`; the last marks the change backfilled.
fn backfill(
    db: &Db,
    plan: &Plan,
    index: &str,
    after: Option<i64>,
    until: Option<i64>,
    rows: u64,
) -> anyhow::Result<Stepped> {
    let reached = |at: Option<i64>| match (at, until) {
        (_, None) => true,
        (Some(at), Some(until)) => at >= until,
        (None, Some(_)) => false,
    };
    let (last, visited, held) = if reached(after) {
        (after, 0, Duration::ZERO)
    } else {
        let began = Instant::now();
        let (last, visited) = db
            .index_backfill(index, after, rows)
            .map_err(|error| backfill_refusal(index, error))?;
        (last.or(after), visited, began.elapsed())
    };
    let done = visited < rows || reached(last);
    db.query(
        "UPDATE __amphora_schema_changes SET phase = ?2, after = ?3 WHERE name = ?1",
        [
            text(&plan.name),
            EValue::from_i64(if done { BACKFILLED } else { WRITE_ONLY }),
            last.map_or(EValue::Null, EValue::from_i64),
        ],
        |_| Ok(true),
    )?;
    Ok(Stepped::Batch { visited, held })
}

/// A backfill's failure as its driver acts on it: rows that break the unique index the change
/// would add ([`BROKEN`]), a batch too large for one transaction (halved), or the error itself.
fn backfill_refusal(index: &str, error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<amphora_engine::LimboError>() {
        Some(amphora_engine::LimboError::Constraint(why)) => {
            api::invalid(BROKEN, &format!("{index} can't be unique: {why}")).into()
        }
        Some(amphora_engine::LimboError::TransactionPageLimit { limit, attempted }) => {
            Refusal::TooLarge {
                what: "online schema change batch dirty pages".into(),
                size: *attempted,
                limit: *limit,
            }
            .into()
        }
        _ => error,
    }
}

/// Write-only to public: the cutover. Only the catalog changes, whatever the table's size.
fn cutover(db: &Db, plan: &Plan, index: &str) -> anyhow::Result<Stepped> {
    let began = Instant::now();
    db.query(
        "DELETE FROM __amphora_index_states WHERE name = ?1",
        [text(index)],
        |_| Ok(true),
    )?;
    db.restage_schema()?;
    finish(db, plan)?;
    migrated(db, plan)?;
    Ok(Stepped::Public {
        held: began.elapsed(),
    })
}

/// Absent to public for a column: its states run together, as its addition is one catalog change
/// (each row written before it reads its constant default), recorded as migrated in the same
/// transaction. A crash leaves the table without the column or with it, never between.
fn add_column(db: &Db, plan: &Plan) -> anyhow::Result<Stepped> {
    let began = Instant::now();
    db.exec(&plan.sql)?;
    migrated(db, plan)?;
    Ok(Stepped::Public {
        held: began.elapsed(),
    })
}

fn migrated(db: &Db, plan: &Plan) -> anyhow::Result<()> {
    db.query(
        "INSERT INTO __amphora_migrations(name, sql) VALUES (?1, ?2)",
        [text(&plan.name), text(&plan.sql)],
        |_| Ok(true),
    )
}

fn finish(db: &Db, plan: &Plan) -> anyhow::Result<()> {
    db.query(
        "DELETE FROM __amphora_schema_changes WHERE name = ?1",
        [text(&plan.name)],
        |_| Ok(true),
    )
}

/// Undoes `plan`'s change, at whatever step it stands: its index dropped (or its column's old
/// definition back), its records gone, the old schema whole. Nothing to undo when it never began.
pub fn abandon(db: &Db, plan: &Plan) -> anyhow::Result<()> {
    if record(db, plan)?.is_none() {
        return Ok(());
    }
    let index = match &plan.adds {
        Adds::Index(index) => index,
        // A drop has no rows to refuse it, and its old values may be cleared already: once begun
        // it only goes on.
        Adds::Column | Adds::DropColumn(_) => return Ok(()),
        Adds::Constraint { column, .. } => return abandon_constraint(db, plan, column),
    };
    db.query(
        "DELETE FROM __amphora_index_states WHERE name = ?1",
        [text(index)],
        |_| Ok(true),
    )?;
    db.restage_schema()?;
    db.exec(&format!("DROP INDEX IF EXISTS {}", quoted(index)))?;
    finish(db, plan)
}

/// The next step of a constraint's change, by its record: begun (write-only), its validation's
/// next batch, or the cutover once every row before it is checked.
fn constraint_step(
    db: &Db,
    plan: &Plan,
    column: &str,
    definition: &ColumnDefinition,
    found: Option<Record>,
    rows: u64,
) -> anyhow::Result<Stepped> {
    let Some(record) = found else {
        return begin_constraint(db, plan, column);
    };
    if record.sql != plan.sql {
        return Err(api::invalid(
            "migration",
            "a change of this name is under way with different SQL",
        )
        .into());
    }
    match record.phase {
        WRITE_ONLY => validate(db, plan, column, definition, &record, rows),
        BACKFILLED => {
            let began = Instant::now();
            db.query(
                "DELETE FROM __amphora_constraint_states WHERE tbl = ?1 AND col = ?2",
                [text(&plan.table), text(column)],
                |_| Ok(true),
            )?;
            db.restage_schema()?;
            finish(db, plan)?;
            migrated(db, plan)?;
            Ok(Stepped::Public {
                held: began.elapsed(),
            })
        }
        phase => Err(anyhow::anyhow!(
            "__amphora_schema_changes: {} is at phase {phase}",
            plan.name
        )),
    }
}

/// The definition `column` has in its table's `CREATE TABLE`, and whether the table has a rowid.
fn column_definition(
    db: &Db,
    table: &str,
    column: &str,
) -> anyhow::Result<(ColumnDefinition, bool)> {
    let mut sql = None;
    db.query(
        "SELECT sql FROM sqlite_schema WHERE type = 'table' AND lower(name) = ?1",
        [text(table)],
        |row| {
            sql = Some(row.get::<String>(0).map_err(unavailable)?);
            Ok(false)
        },
    )?;
    let refuse =
        |why: String| -> anyhow::Error { api::invalid("online schema change", &why).into() };
    let sql = sql.ok_or_else(|| refuse(format!("no table {table}")))?;
    let mut parser = Parser::new(sql.as_bytes());
    let Some(Cmd::Stmt(Stmt::CreateTable {
        body: CreateTableBody::ColumnsAndConstraints {
            columns, options, ..
        },
        ..
    })) = parser.next_cmd().map_err(|e| refuse(e.to_string()))?
    else {
        return Err(refuse(format!("{table} is not a table of columns")));
    };
    let definition = columns
        .into_iter()
        .find(|c| c.col_name.as_str().to_ascii_lowercase() == column)
        .ok_or_else(|| refuse(format!("no column {column} in {table}")))?;
    Ok((definition, options.without_rowid_text.is_none()))
}

/// Whether `new` is exactly `old` with NOT NULL or CHECK constraints after its own: the same type
/// and constraints, then only the added ones, so removing them gives `old` back (the change can be
/// undone by redefining the column as it was). NOT NULL is added only to a column without it.
fn adds_only_constraints(old: &ColumnDefinition, new: &ColumnDefinition) -> bool {
    let kept = old.constraints.len();
    let (Some(head), Some(added)) = (new.constraints.get(..kept), new.constraints.get(kept..))
    else {
        return false;
    };
    let not_null = |c: &&amphora_engine_parser::ast::NamedColumnConstraint| {
        matches!(
            c.constraint,
            ColumnConstraint::NotNull {
                nullable: false,
                ..
            }
        )
    };
    old.col_type == new.col_type
        && head.len() == old.constraints.len()
        && head
            .iter()
            .zip(&old.constraints)
            .all(|(new, old)| same_constraint(new, old))
        && !added.is_empty()
        && added.iter().all(|c| constrains(&c.constraint))
        && !(old.constraints.iter().any(|c| not_null(&c)) && added.iter().any(|c| not_null(&c)))
}

/// Whether two column constraints say the same: equal, or CHECKs of one name over expressions
/// that render the same (the catalog may keep a table's SQL with the expression's source text
/// spaced differently from the migration's).
fn same_constraint(
    new: &amphora_engine_parser::ast::NamedColumnConstraint,
    old: &amphora_engine_parser::ast::NamedColumnConstraint,
) -> bool {
    match (&new.constraint, &old.constraint) {
        (ColumnConstraint::Check { expr: a, .. }, ColumnConstraint::Check { expr: b, .. }) => {
            new.name == old.name && a.to_string() == b.to_string()
        }
        _ => new == old,
    }
}

/// Absent to write-only for a constraint, in one transaction: the column marked unvalidated (its
/// old definition kept beside the mark, to undo it by), then redefined with the constraint, which
/// writes are checked against from here and which the engine doesn't scan the table for (the
/// mark says the batches will); the rows the validation must reach are those up to the largest
/// rowid now.
fn begin_constraint(db: &Db, plan: &Plan, column: &str) -> anyhow::Result<Stepped> {
    let Adds::Constraint { definition, .. } = &plan.adds else {
        return Err(anyhow::anyhow!("{} adds no constraint", plan.name));
    };
    let (old, rowid) = column_definition(db, &plan.table, column)?;
    if !rowid {
        return Err(api::invalid(
            "online schema change",
            "a WITHOUT ROWID table has no rowid order to validate by",
        )
        .into());
    }
    if old
        .constraints
        .iter()
        .any(|c| matches!(c.constraint, ColumnConstraint::Generated { .. }))
    {
        return Err(api::invalid(
            "online schema change",
            "a generated column's constraint is checked by computing it for every row at once: not built online",
        )
        .into());
    }
    if !adds_only_constraints(&old, definition) {
        return Err(api::invalid(
            "online schema change",
            &format!(
                "the new definition must be the column's own ({old}) with NOT NULL or CHECK after it"
            ),
        )
        .into());
    }
    db.exec(TABLES)?;
    db.query(
        "INSERT INTO __amphora_constraint_states(tbl, col, old) VALUES (?1, ?2, ?3)",
        [text(&plan.table), text(column), text(&old.to_string())],
        |_| Ok(true),
    )?;
    db.restage_schema()?;
    db.exec_as_change(&plan.sql)?;
    let mut until = None;
    db.query_owned(
        &format!("SELECT max(rowid) FROM {}", quoted(&plan.table)),
        [],
        |row| {
            until = optional(row, 0)?;
            Ok(false)
        },
    )?;
    db.query(
        "INSERT INTO __amphora_schema_changes(name, sql, idx, tbl, phase, until) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        [
            text(&plan.name),
            text(&plan.sql),
            text(column),
            text(&plan.table),
            EValue::from_i64(WRITE_ONLY),
            until.map_or(EValue::Null, EValue::from_i64),
        ],
        |_| Ok(true),
    )?;
    // The redefinition gave the writer's schema the column without its mark: read it again, so
    // the writer's own reads don't rely on the constraint before it is validated either.
    db.restage_schema()?;
    Ok(Stepped::WriteOnly)
}

/// What a row breaking `definition`'s NOT NULL and CHECK constraints makes true, as SQL over the
/// column's table: a NULL under NOT NULL, or a CHECK that is false (a NULL CHECK passes, as
/// SQLite's does). The column's earlier constraints hold already; checking them again costs a
/// little and keeps one expression.
fn breaks(column: &str, definition: &ColumnDefinition) -> String {
    definition
        .constraints
        .iter()
        .filter_map(|c| match &c.constraint {
            ColumnConstraint::NotNull {
                nullable: false, ..
            } => Some(format!("{} IS NULL", quoted(column))),
            ColumnConstraint::Check { expr, .. } => Some(format!("coalesce(NOT ({expr}), 0)")),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// One validation batch: the rows after the last one checked, up to the largest rowid the table
/// held when the constraint went write-only, at most `rows`. A row that breaks the constraint
/// stops the change ([`BROKEN`]); the last batch marks it validated.
fn validate(
    db: &Db,
    plan: &Plan,
    column: &str,
    definition: &ColumnDefinition,
    record: &Record,
    rows: u64,
) -> anyhow::Result<Stepped> {
    let reached = |at: Option<i64>| match (at, record.until) {
        (_, None) => true,
        (Some(at), Some(until)) => at >= until,
        (None, Some(_)) => false,
    };
    let (last, visited, held) = match (reached(record.after), record.until) {
        (false, Some(until)) => {
            let began = Instant::now();
            // Saturating is the meaning: a last rowid of i64::MAX is at or past `until`, so a batch
            // never starts there.
            let from = match record.after {
                Some(after) => after.saturating_add(1),
                None => i64::MIN,
            };
            let mut seen = (None, 0u64, None);
            db.query_owned(
                &format!(
                    "SELECT max(r), count(*), min(CASE WHEN bad THEN r END) FROM (SELECT rowid AS r, ({}) AS bad FROM {} WHERE rowid >= ?1 AND rowid <= ?2 ORDER BY rowid LIMIT ?3)",
                    breaks(column, definition),
                    quoted(&plan.table)
                ),
                [
                    EValue::from_i64(from),
                    EValue::from_i64(until),
                    EValue::from_i64(i64::try_from(rows).unwrap_or(i64::MAX)),
                ],
                |row| {
                    seen = (
                        optional(row, 0)?,
                        u64::try_from(row.get::<i64>(1).map_err(unavailable)?).unwrap_or(0),
                        optional(row, 2)?,
                    );
                    Ok(false)
                },
            )?;
            let (last, visited, broken) = seen;
            if let Some(broken) = broken {
                return Err(api::invalid(
                    BROKEN,
                    &format!(
                        "row {broken} of {} breaks the constraint added to {column}",
                        plan.table
                    ),
                )
                .into());
            }
            (last.or(record.after), visited, began.elapsed())
        }
        _ => (record.after, 0, Duration::ZERO),
    };
    let done = visited < rows || reached(last);
    db.query(
        "UPDATE __amphora_schema_changes SET phase = ?2, after = ?3 WHERE name = ?1",
        [
            text(&plan.name),
            EValue::from_i64(if done { BACKFILLED } else { WRITE_ONLY }),
            last.map_or(EValue::Null, EValue::from_i64),
        ],
        |_| Ok(true),
    )?;
    Ok(Stepped::Batch { visited, held })
}

/// Undoes a constraint's change: the column redefined as it was (its mark still on, so the engine
/// doesn't scan the table for its old constraints, which every row keeps), then the mark and the
/// record gone, in the caller's transaction.
fn abandon_constraint(db: &Db, plan: &Plan, column: &str) -> anyhow::Result<()> {
    let mut old = None;
    db.query(
        "SELECT old FROM __amphora_constraint_states WHERE tbl = ?1 AND col = ?2",
        [text(&plan.table), text(column)],
        |row| {
            old = Some(row.get::<String>(0).map_err(unavailable)?);
            Ok(false)
        },
    )?;
    if let Some(old) = old {
        db.exec_as_change(&format!(
            "ALTER TABLE {} ALTER COLUMN {} TO {old}",
            quoted(&plan.table),
            quoted(column)
        ))?;
        db.query(
            "DELETE FROM __amphora_constraint_states WHERE tbl = ?1 AND col = ?2",
            [text(&plan.table), text(column)],
            |_| Ok(true),
        )?;
        db.restage_schema()?;
    }
    finish(db, plan)
}

/// The next step of a drop's change, by its record: begun (the column hidden), its clear's next
/// batch, or the record once every row before it is cleared.
fn drop_step(
    db: &Db,
    plan: &Plan,
    column: &str,
    found: Option<Record>,
    rows: u64,
) -> anyhow::Result<Stepped> {
    let Some(record) = found else {
        return begin_drop(db, plan, column);
    };
    if record.sql != plan.sql {
        return Err(api::invalid(
            "migration",
            "a change of this name is under way with different SQL",
        )
        .into());
    }
    match record.phase {
        WRITE_ONLY => clear(db, plan, column, &record, rows),
        BACKFILLED => {
            let began = Instant::now();
            finish(db, plan)?;
            migrated(db, plan)?;
            Ok(Stepped::Public {
                held: began.elapsed(),
            })
        }
        phase => Err(anyhow::anyhow!(
            "__amphora_schema_changes: {} is at phase {phase}",
            plan.name
        )),
    }
}

/// The switch-over of a drop, in one transaction: the column named in `__amphora_dropped_columns`,
/// so from this commit no name finds it, `*` and inserts pass it and every write stores NULL in it
/// (PostgreSQL's `attisdropped`); the rows the clear must reach are those up to the largest rowid
/// now. Refused before anything changes: what the engine's own `DROP COLUMN` refuses (its
/// statement is prepared, never run: a key, UNIQUE, an index, a CHECK, a foreign key, a generated
/// column, a view or a trigger naming it, the only column), a WITHOUT ROWID table (no rowid order
/// to clear by), a table with triggers (the clear's updates would fire them) and a table with no
/// other plain column for the clear to rewrite rows by.
fn begin_drop(db: &Db, plan: &Plan, column: &str) -> anyhow::Result<Stepped> {
    let refuse = |why: &str| -> anyhow::Error { api::invalid("online schema change", why).into() };
    let (_, rowid) = column_definition(db, &plan.table, column)?;
    if !rowid {
        return Err(refuse(
            "a WITHOUT ROWID table has no rowid order to clear a dropped column by",
        ));
    }
    let mut triggers = false;
    db.query(
        "SELECT 1 FROM sqlite_schema WHERE type = 'trigger' AND lower(tbl_name) = ?1",
        [text(&plan.table)],
        |_| {
            triggers = true;
            Ok(false)
        },
    )?;
    if triggers {
        return Err(refuse(
            "the table has triggers, which the batches clearing the dropped column would fire",
        ));
    }
    db.column_count(&plan.sql)
        .map_err(|error| refuse(&error.to_string()))?;
    if rewriting_column(db, &plan.table, column)?.is_none() {
        return Err(refuse(
            "the table has no other plain column for the batches to rewrite its rows by",
        ));
    }
    db.exec(TABLES)?;
    db.query(
        "INSERT INTO __amphora_dropped_columns(tbl, col) VALUES (?1, ?2)",
        [text(&plan.table), text(column)],
        |_| Ok(true),
    )?;
    let mut until = None;
    db.query_owned(
        &format!("SELECT max(rowid) FROM {}", quoted(&plan.table)),
        [],
        |row| {
            until = optional(row, 0)?;
            Ok(false)
        },
    )?;
    db.query(
        "INSERT INTO __amphora_schema_changes(name, sql, idx, tbl, phase, until) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        [
            text(&plan.name),
            text(&plan.sql),
            text(column),
            text(&plan.table),
            EValue::from_i64(WRITE_ONLY),
            until.map_or(EValue::Null, EValue::from_i64),
        ],
        |_| Ok(true),
    )?;
    db.restage_schema()?;
    // What the engine's DROP COLUMN doesn't check, prepared (never run) against the schema without
    // the column, in this transaction: a view naming it, and a CHECK of another column naming it
    // (an insert compiles every CHECK). A failure refuses the drop and its transaction is undone.
    let mut views = Vec::new();
    db.query(
        "SELECT name FROM sqlite_schema WHERE type = 'view'",
        [],
        |row| {
            views.push(row.get::<String>(0).map_err(unavailable)?);
            Ok(true)
        },
    )?;
    for view in views {
        db.column_count(&format!("SELECT * FROM {}", quoted(&view)))
            .map_err(|error| refuse(&format!("view {view} names the column: {error}")))?;
    }
    db.column_count(&format!(
        "INSERT INTO {} DEFAULT VALUES",
        quoted(&plan.table)
    ))
    .map_err(|error| refuse(&format!("a constraint names the column: {error}")))?;
    Ok(Stepped::WriteOnly)
}

/// A column of `table` other than `dropping` an update can set to itself to rewrite a row: shown
/// (so not dropped), not generated, not a key. Rewriting a row stores NULL in its dropped columns.
fn rewriting_column(db: &Db, table: &str, dropping: &str) -> anyhow::Result<Option<String>> {
    let mut found = None;
    // PRAGMA table_xinfo's rows: cid, name, type, notnull, dflt_value, pk, hidden (0 for a plain
    // column; SQLite's documented format).
    db.query_owned(
        &format!("PRAGMA table_xinfo({})", quoted(table)),
        [],
        |row| {
            let pk = row.get::<i64>(5).map_err(unavailable)?;
            let hidden = row.get::<i64>(6).map_err(unavailable)?;
            let name = row.get::<String>(1).map_err(unavailable)?;
            if pk == 0 && hidden == 0 && !name.eq_ignore_ascii_case(dropping) {
                found = Some(name);
                return Ok(false);
            }
            Ok(true)
        },
    )?;
    Ok(found)
}

/// One clearing batch: the rows after the last one cleared, up to the largest rowid the table held
/// at the switch-over, at most `rows`, each rewritten (an update stores NULL in a dropped column).
/// The last batch marks the change cleared.
fn clear(
    db: &Db,
    plan: &Plan,
    dropping: &str,
    record: &Record,
    rows: u64,
) -> anyhow::Result<Stepped> {
    let reached = |at: Option<i64>| match (at, record.until) {
        (_, None) => true,
        (Some(at), Some(until)) => at >= until,
        (None, Some(_)) => false,
    };
    let (last, visited, held) = match (reached(record.after), record.until) {
        (false, Some(until)) => {
            let began = Instant::now();
            // Saturating is the meaning: a last rowid of i64::MAX is at or past `until`.
            let from = match record.after {
                Some(after) => after.saturating_add(1),
                None => i64::MIN,
            };
            let mut seen = (None, 0u64);
            db.query_owned(
                &format!(
                    "SELECT max(r), count(*) FROM (SELECT rowid AS r FROM {} WHERE rowid >= ?1 AND rowid <= ?2 ORDER BY rowid LIMIT ?3)",
                    quoted(&plan.table)
                ),
                [
                    EValue::from_i64(from),
                    EValue::from_i64(until),
                    EValue::from_i64(i64::try_from(rows).unwrap_or(i64::MAX)),
                ],
                |row| {
                    seen = (
                        optional(row, 0)?,
                        u64::try_from(row.get::<i64>(1).map_err(unavailable)?).unwrap_or(0),
                    );
                    Ok(false)
                },
            )?;
            let (last, visited) = seen;
            if let Some(last) = last {
                let column = rewriting_column(db, &plan.table, dropping)?.ok_or_else(|| {
                    anyhow::anyhow!("{} has no plain column to rewrite rows by", plan.table)
                })?;
                db.query_owned(
                    &format!(
                        "UPDATE {table} SET {column} = {column} WHERE rowid >= ?1 AND rowid <= ?2",
                        table = quoted(&plan.table),
                        column = quoted(&column)
                    ),
                    [EValue::from_i64(from), EValue::from_i64(last)],
                    |_| Ok(true),
                )
                .map_err(clear_refusal)?;
            }
            (last.or(record.after), visited, began.elapsed())
        }
        _ => (record.after, 0, Duration::ZERO),
    };
    let done = visited < rows || reached(last);
    db.query(
        "UPDATE __amphora_schema_changes SET phase = ?2, after = ?3 WHERE name = ?1",
        [
            text(&plan.name),
            EValue::from_i64(if done { BACKFILLED } else { WRITE_ONLY }),
            last.map_or(EValue::Null, EValue::from_i64),
        ],
        |_| Ok(true),
    )?;
    Ok(Stepped::Batch { visited, held })
}

/// A clearing batch's failure: too many dirty pages for one transaction is halved by the driver.
fn clear_refusal(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<amphora_engine::LimboError>() {
        Some(amphora_engine::LimboError::TransactionPageLimit { limit, attempted }) => {
            Refusal::TooLarge {
                what: "online schema change batch dirty pages".into(),
                size: *attempted,
                limit: *limit,
            }
            .into()
        }
        _ => error,
    }
}

impl Database {
    /// Builds the index `migration` creates (one `CREATE [UNIQUE] INDEX`) online, by F1's states
    /// (CP-T1.12, docs/design/online-schema.md): writes go on while its backfill runs, and the
    /// writer is held for the cutover to public only as long as a catalog change takes. A column
    /// (one `ALTER TABLE … ADD COLUMN`) is added in one catalog change, its states run together;
    /// one whose addition would read every row is refused. A NOT NULL or CHECK (one `ALTER TABLE …
    /// ALTER COLUMN c TO` its definition plus the constraint) is checked on writes, then validated
    /// against the rows before it in batches, then made public by a catalog change. A column
    /// (one `ALTER TABLE … DROP COLUMN`) is hidden by a catalog change, its old values cleared
    /// in batches, then its drop recorded. Applied
    /// once by name, as [`Database::migrate`]. A deadline that passes leaves the change at a
    /// step, invisible to reads; calling again with the same migration carries it on. Rows that
    /// break a unique index or the constraint undo the change and are refused.
    pub fn change_schema(
        &self,
        migration: Migration,
        until: Instant,
    ) -> Result<SchemaChange, Refusal> {
        api::family(&migration.name)?;
        validate_sql(&migration.sql, false)?;
        let plan = Arc::new(Plan::parse(&migration)?);
        let database = self.resolve(until)?;
        if database.remote_client().is_some() {
            return Err(api::invalid(
                "online schema change",
                "it runs on the database's owner",
            ));
        }
        let mut sizer = Sizer::new();
        let mut change = SchemaChange::default();
        // Each step moves the change on (a phase, or its backfill's rowid), and every call
        // refuses past `until`: the loop ends on the change's end or the deadline.
        loop {
            let rows = sizer.rows();
            let stepping = Arc::clone(&plan);
            let began = Instant::now();
            let stepped = database.call_db(until, move |db| step(db, &stepping, rows));
            let took = began.elapsed();
            match stepped {
                Ok(Stepped::Already) => return Ok(change),
                Ok(Stepped::Public { held }) => {
                    change.applied = true;
                    change.cutover = Some(held);
                    return Ok(change);
                }
                Ok(Stepped::Batch { visited, held }) => {
                    if visited > 0 {
                        change.batches = change.batches.saturating_add(1);
                        change.rows = change.rows.saturating_add(visited);
                        sizer.learn(visited, held, took.saturating_sub(held));
                    }
                }
                Ok(Stepped::DeleteOnly | Stepped::WriteOnly) => {}
                Err(refusal @ Refusal::TooLarge { .. }) => {
                    if sizer.halve().is_none() {
                        return Err(refusal);
                    }
                }
                Err(Refusal::Invalid { what, why }) if what == BROKEN => {
                    let undoing = Arc::clone(&plan);
                    database.call_db(until, move |db| abandon(db, &undoing))?;
                    return Err(Refusal::Invalid { what, why });
                }
                Err(refusal) => return Err(refusal),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t_t1_12_sizer_starts_at_a_row_and_at_most_doubles() {
        // CP-T1.12: a batch that held the writer 1 ms and waited 10 ms grows only to twice its
        // rows; one that held 10 ms and waited 1 ms shrinks to a tenth; never under a row.
        let mut sizer = Sizer::new();
        assert_eq!(sizer.rows(), 1);
        sizer.learn(1, Duration::from_millis(1), Duration::from_millis(10));
        assert_eq!(sizer.rows(), 2);
        sizer.learn(1000, Duration::from_millis(10), Duration::from_millis(1));
        assert_eq!(sizer.rows(), 100);
        sizer.learn(100, Duration::from_millis(10), Duration::ZERO);
        assert_eq!(sizer.rows(), 1);
    }

    #[test]
    fn t_t1_12_sizer_holds_the_writer_as_long_as_it_waited() {
        // CP-T1.12: a batch that held the writer 4 ms and waited 3 ms besides asks 3/4 of its rows
        // next; one that held 2 ms and waited 3 ms asks 3/2 of them.
        let mut sizer = Sizer::new();
        sizer.learn(400, Duration::from_millis(4), Duration::from_millis(3));
        assert_eq!(sizer.rows(), 300);
        sizer.learn(300, Duration::from_millis(2), Duration::from_millis(3));
        assert_eq!(sizer.rows(), 450);
    }

    #[test]
    fn t_t1_12_sizer_halves_to_a_row_then_refuses() {
        let mut sizer = Sizer::new();
        sizer.learn(3, Duration::ZERO, Duration::from_millis(1));
        assert_eq!(sizer.rows(), 6);
        assert_eq!(sizer.halve(), Some(3));
        assert_eq!(sizer.halve(), Some(1));
        assert_eq!(sizer.halve(), None);
    }

    #[test]
    fn t_t1_12_a_plan_is_one_create_index_without_if_not_exists() {
        let plan = |sql: &str| {
            Plan::parse(&Migration {
                name: "m".into(),
                sql: sql.into(),
            })
        };
        let made = plan("CREATE UNIQUE INDEX \"Ix\" ON T(a)").unwrap();
        assert_eq!(
            (&made.adds, made.table.as_str()),
            (&Adds::Index("ix".into()), "t")
        );
        assert!(plan("CREATE INDEX IF NOT EXISTS ix ON t(a)").is_err());
        assert!(plan("CREATE INDEX ix ON t(a); CREATE INDEX iy ON t(b)").is_err());
        assert!(plan("CREATE TABLE t(a)").is_err());
        assert!(plan("CREATE INDEX aux.ix ON t(a)").is_err());
    }

    #[test]
    fn t_t1_12_a_column_plan_is_an_add_column_that_reads_no_row() {
        // CP-T1.12: ADD COLUMN, with a constant default, NOT NULL or CHECK, is one catalog change;
        // a generated column with NOT NULL or CHECK (checked against every row) and every other
        // ALTER TABLE (rewrites or checks every row) are refused.
        let plan = |sql: &str| {
            Plan::parse(&Migration {
                name: "m".into(),
                sql: sql.into(),
            })
        };
        let made =
            plan("ALTER TABLE T ADD COLUMN c INTEGER NOT NULL DEFAULT 0 CHECK (c >= 0)").unwrap();
        assert_eq!((&made.adds, made.table.as_str()), (&Adds::Column, "t"));
        assert!(plan("ALTER TABLE t ADD COLUMN g AS (a + 1)").is_ok());
        assert!(plan("ALTER TABLE t ADD COLUMN g AS (a + 1) NOT NULL").is_err());
        assert!(plan("ALTER TABLE t ADD COLUMN g AS (a + 1) CHECK (g > 0)").is_err());
        assert_eq!(
            plan("ALTER TABLE T DROP COLUMN A").unwrap().adds,
            Adds::DropColumn("a".into())
        );
        assert!(plan("ALTER TABLE t RENAME TO u").is_err());
        assert!(plan("ALTER TABLE t RENAME COLUMN a TO b").is_err());
        assert!(plan("ALTER TABLE aux.t ADD COLUMN c").is_err());
    }

    #[test]
    fn t_t1_12_a_constraint_plan_keeps_the_name_and_adds_not_null_or_check() {
        // CP-T1.12: ALTER COLUMN c TO c's definition with NOT NULL or CHECK is a constraint's
        // change; a rename or a definition adding neither is refused; a definition is the old one
        // plus constraints after it only when type and earlier constraints match.
        let plan = |sql: &str| {
            Plan::parse(&Migration {
                name: "m".into(),
                sql: sql.into(),
            })
        };
        let made = plan("ALTER TABLE T ALTER COLUMN \"A\" TO a INTEGER NOT NULL").unwrap();
        assert!(matches!(&made.adds, Adds::Constraint { column, .. } if column == "a"));
        assert!(plan("ALTER TABLE t ALTER COLUMN a TO b INTEGER NOT NULL").is_err());
        assert!(plan("ALTER TABLE t ALTER COLUMN a TO a INTEGER DEFAULT 1").is_err());
        let definition = |sql: &str| match plan(sql).unwrap().adds {
            Adds::Constraint { definition, .. } => definition,
            other => panic!("{other:?}"),
        };
        let old = definition("ALTER TABLE t ALTER COLUMN a TO a INTEGER CHECK (a > 0)");
        let adds = |sql: &str| adds_only_constraints(&old, &definition(sql));
        assert!(adds(
            "ALTER TABLE t ALTER COLUMN a TO a INTEGER CHECK (a>0) NOT NULL"
        ));
        assert!(adds(
            "ALTER TABLE t ALTER COLUMN a TO a INTEGER CHECK (a > 0) CHECK (a < 9)"
        ));
        assert!(!adds(
            "ALTER TABLE t ALTER COLUMN a TO a TEXT CHECK (a > 0) NOT NULL"
        ));
        assert!(!adds(
            "ALTER TABLE t ALTER COLUMN a TO a INTEGER NOT NULL CHECK (a > 0)"
        ));
        assert!(!adds(
            "ALTER TABLE t ALTER COLUMN a TO a INTEGER CHECK (a > 1) NOT NULL"
        ));
        let not_null = definition("ALTER TABLE t ALTER COLUMN a TO a INTEGER NOT NULL");
        assert!(!adds_only_constraints(
            &not_null,
            &definition("ALTER TABLE t ALTER COLUMN a TO a INTEGER NOT NULL NOT NULL")
        ));
    }
}
