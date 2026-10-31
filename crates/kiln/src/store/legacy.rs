//! U05: a snapshot-pinned, read-only SQLite importer. Only Amphora acknowledges writes. Rows and
//! the keyset cursor commit together, so interruption repeats no row and loses no row. The source
//! logical digest fences a resumed import from a different snapshot, including WAL-only writes.

use std::collections::HashSet;
use std::path::Path;

use anyhow::Context as _;
use amphora::database::{Database, Restore};
use amphora_api::{Statement, Value, restore::RestorePage};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, types::ValueRef};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::deadline;

const PAGE: usize = 128;
const PAGE_BYTES: usize = 4 << 20;
const ROW_BYTES: usize = 32 << 20;

#[derive(Clone)]
struct Object { kind: String, name: String, table: String, sql: String }
#[derive(Clone)]
struct Table { name: String, sql: String, columns: Vec<String>, keys: Vec<String>, positions: Vec<usize>, rowid: bool }
#[derive(Clone, Serialize, Deserialize)]
struct Cursor(Vec<Value>);
struct Page { rows: Vec<Vec<Value>>, next: Cursor }
struct Legacy { connection: Connection, objects: Vec<Object>, tables: Vec<Table>, log: Option<crate::replication::legacy_read::Source> }

fn quote(name: &str) -> String { format!("\"{}\"", name.replace('"', "\"\"")) }
fn owned(value: ValueRef<'_>) -> anyhow::Result<Value> {
    Ok(match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(v) => Value::Integer(v),
        ValueRef::Real(v) => Value::Real(v),
        ValueRef::Text(v) => Value::Text(std::str::from_utf8(v)?.to_owned()),
        ValueRef::Blob(v) => Value::Blob(v.to_vec()),
    })
}
fn old(value: &Value) -> rusqlite::types::Value {
    match value { Value::Null => rusqlite::types::Value::Null, Value::Integer(v) => (*v).into(), Value::Real(v) => (*v).into(), Value::Text(v) => v.clone().into(), Value::Blob(v) => v.clone().into() }
}
fn size(value: &Value) -> usize { match value { Value::Text(v) => v.len(), Value::Blob(v) => v.len(), _ => 8 } }

impl Legacy {
    fn open(path: &Path) -> anyhow::Result<Self> {
        // U05: production activation uses only the supervised-offline, OS-readonly fenced
        // snapshot. Keep normal WAL handling: immutable=1 would silently ignore its WAL.
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA query_only=ON; BEGIN")?;
        // This first read establishes the source WAL snapshot for both identity and all pages.
        let objects = connection.prepare("SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY rowid")?.query_map([], |r| Ok(Object { kind:r.get(0)?, name:r.get(1)?, table:r.get(2)?, sql:r.get(3)? }))?.collect::<rusqlite::Result<Vec<_>>>()?;
        anyhow::ensure!(objects.len() <= 8192, "legacy SQL schema exceeds 8192 objects");
        // U15: compare the durable log against this exact pinned SQL snapshot. A committed tail
        // that the database lacks must be recovered offline before its state can be migrated.
        let (applied,term) = if objects.iter().any(|o|o.kind=="table"&&o.name=="cluster_state") {
            let (applied,term)=connection.query_row("SELECT applied,term FROM cluster_state WHERE one=1",[],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?))).optional()?.unwrap_or((0,0));
            (u64::try_from(applied)?,u64::try_from(term)?)
        } else { (0,0) };
        let log=crate::replication::legacy::check(path,applied,term)?;
        if objects.iter().any(|o| o.kind == "table" && o.name == "kiln_meta") {
            let epoch: Option<String> = connection.query_row("SELECT value FROM kiln_meta WHERE key='min_reader_epoch'", [], |r| r.get(0)).optional()?;
            let need = epoch.and_then(|e| e.parse::<u32>().ok()).unwrap_or(0);
            anyhow::ensure!(need <= super::migrations::SCHEMA_EPOCH, "legacy kiln.db needs schema epoch {need}; this build reads {}", super::migrations::SCHEMA_EPOCH);
        }
        let invalid: Option<String> = connection.prepare("PRAGMA foreign_key_check")?.query_map([], |r| r.get(0))?.next().transpose()?;
        anyhow::ensure!(invalid.is_none(), "legacy SQL source contains a foreign-key violation in {}", invalid.unwrap_or_default());
        anyhow::ensure!(!objects.iter().any(|o| o.name.starts_with("__amphora_") || o.name.starts_with("__kiln_")), "legacy SQL source uses an Amphora-reserved schema name");
        let virtuals: HashSet<String> = objects.iter().filter(|o| o.kind == "table" && o.sql.trim_start().to_ascii_uppercase().starts_with("CREATE VIRTUAL TABLE")).map(|o| o.name.clone()).collect();
        anyhow::ensure!(virtuals.iter().all(|name| name == "ci_log_index"), "legacy SQL source contains an unsupported virtual table; its data must be migrated explicitly");
        let kept = |o: &Object| !o.name.starts_with("sqlite_") && !o.name.starts_with("__amphora_") && !o.name.starts_with("__kiln_")
            && !["cluster_state", "cluster_conf", "cluster_mark", "ci_log_indexed"].contains(&o.name.as_str())
            && !virtuals.iter().any(|v| o.table == *v || o.table.starts_with(&format!("{v}_")));
        let objects: Vec<_> = objects.into_iter().filter(kept).collect();
        let mut tables = Vec::new();
        for object in objects.iter().filter(|o| o.kind == "table") {
            let columns: Vec<(String, i64, i64)> = connection.prepare(&format!("PRAGMA table_xinfo({})", quote(&object.name)))?.query_map([], |r| Ok((r.get(1)?, r.get(5)?, r.get(6)?)))?.collect::<rusqlite::Result<_>>()?;
            anyhow::ensure!(!columns.is_empty() && columns.len() <= 1000, "legacy table {} has an unsupported column count", object.name);
            // Generated columns are recomputed by the product; hidden virtual columns were excluded.
            let columns: Vec<_> = columns.into_iter().filter(|(_, _, hidden)| *hidden == 0).collect();
            let without_rowid: bool = connection.query_row("SELECT wr FROM pragma_table_list WHERE schema='main' AND name=?1", [&object.name], |r| r.get(0))?;
            let rowid = !without_rowid;
            let (keys, positions) = if rowid {
                let alias = ["rowid", "_rowid_", "oid"].into_iter().find(|alias| !columns.iter().any(|(name, _, _)| name.eq_ignore_ascii_case(alias))).context("legacy table shadows every rowid alias")?;
                (vec![alias.to_owned()], vec![0])
            } else {
                let mut primary: Vec<_> = columns.iter().enumerate().filter(|(_, (_, pk, _))| *pk > 0).collect();
                primary.sort_by_key(|(_, (_, pk, _))| *pk);
                anyhow::ensure!(!primary.is_empty(), "legacy WITHOUT ROWID table lacks its primary key");
                (primary.iter().map(|(_, (name, _, _))| name.clone()).collect(), primary.iter().map(|(n, _)| *n).collect())
            };
            tables.push(Table { name:object.name.clone(), sql:object.sql.clone(), columns:columns.into_iter().map(|(name, _, _)| name).collect(), keys, positions, rowid });
        }
        Ok(Self { connection, objects, tables, log })
    }

    fn page(&self, table: &Table, cursor: Option<&Cursor>) -> anyhow::Result<Option<Page>> {
        let names: Vec<_> = table.columns.iter().map(|c| quote(c)).collect();
        let keys = table.keys.iter().map(|k| quote(k)).collect::<Vec<_>>().join(",");
        let selected = if table.rowid { format!("{keys},{}", names.join(",")) } else { names.join(",") };
        let values = cursor.map(|c| c.0.iter().map(old).collect::<Vec<_>>()).unwrap_or_default();
        let filter = cursor.map_or(String::new(), |_| format!(" WHERE ({keys}) > ({})", (1..=values.len()).map(|n| format!("?{n}")).collect::<Vec<_>>().join(",")));
        let mut statement = self.connection.prepare(&format!("SELECT {selected} FROM {}{filter} ORDER BY {keys} LIMIT {PAGE}", quote(&table.name)))?;
        let n = statement.column_count();
        let mut rows = statement.query(rusqlite::params_from_iter(values))?;
        let mut output = Vec::new();
        let mut used = 0usize;
        while let Some(row) = rows.next()? {
            let values = (0..n).map(|n| owned(row.get_ref(n)?)).collect::<anyhow::Result<Vec<_>>>()?;
            let bytes = values.iter().try_fold(0usize, |sum, value| sum.checked_add(size(value)).context("legacy row size overflow"))?;
            anyhow::ensure!(bytes <= ROW_BYTES, "legacy table {} contains a row above the bounded 32 MiB SQL request limit", table.name);
            used = used.checked_add(bytes).context("legacy page size overflow")?;
            output.push(values);
            if used >= PAGE_BYTES { break; }
        }
        let Some(last) = output.last() else { return Ok(None); };
        let cursor = Cursor(table.positions.iter().map(|position| last.get(*position).cloned().context("legacy cursor outside result")).collect::<anyhow::Result<_>>()?);
        Ok(Some(Page { rows:output, next:cursor }))
    }

    fn identity(&self) -> anyhow::Result<String> {
        let mut digest = Sha256::new();
        digest.update(b"kiln-sql-snapshot-v1");
        let mut part = |tag: u8, bytes: &[u8]| -> anyhow::Result<()> {
            digest.update([tag]);
            digest.update(u64::try_from(bytes.len())?.to_le_bytes());
            digest.update(bytes);
            Ok(())
        };
        for object in &self.objects {
            part(0, &postcard::to_allocvec(&(&object.kind, &object.name, &object.table, &object.sql))?)?;
        }
        for table in &self.tables {
            part(1, table.name.as_bytes())?;
            let mut cursor = None;
            while let Some(page) = self.page(table, cursor.as_ref())? {
                for row in page.rows { part(2, &postcard::to_allocvec(&row)?)?; }
                cursor = Some(page.next);
            }
            part(3, &[])?;
        }
        Ok(hex::encode(digest.finalize()))
    }
}

#[derive(Serialize, Deserialize)]
enum Position { Rows { table: usize, after: Option<Cursor> }, Objects, Finished }

pub(super) fn identity(source: &Path) -> anyhow::Result<String> {
    let legacy = Legacy::open(source)?;
    let identity = format!("main-sql-v1:{}", legacy.identity()?);
    if let Some(log) = &legacy.log { log.verify()?; }
    Ok(identity)
}

pub(super) fn import(destination: &Path, source: &Path, expected: Option<&str>) -> anyhow::Result<Database> {
    // A completed restore retires its source. Startup should not hash gigabytes of that input
    // again, and later changes to a retired file must never overwrite the live product database.
    if destination.is_file() {
        match Database::open(destination, deadline()) {
            Ok(database) => {
                if let Some(state) = database.restore_progress(deadline())? && state.done {
                    anyhow::ensure!(expected.is_none_or(|source| source == state.source), "U05 completed Amphora restore and retired source disagree; possible writes accepted by an older build must be reconciled, never discarded");
                    return Ok(database);
                }
            }
            Err(amphora_api::Refusal::Invalid { what, .. }) if what == "database" => {}
            Err(error) => return Err(error.into()),
        }
    }
    import_checked(destination, source, None, expected)
}

#[cfg(test)]
fn import_pages(destination: &Path, source: &Path, stop_after: Option<usize>) -> anyhow::Result<Database> {
    import_checked(destination, source, stop_after, None)
}

fn import_checked(destination: &Path, source: &Path, stop_after: Option<usize>, expected: Option<&str>) -> anyhow::Result<Database> {
    let legacy = Legacy::open(source).with_context(|| format!("read legacy {}", source.display()))?;
    let identity = format!("main-sql-v1:{}", legacy.identity()?);
    anyhow::ensure!(expected.is_none_or(|source| source == identity), "U05 preserved source differs from its offline cutover identity; retain it and reconcile before import");
    let restore = Restore::open(destination, &identity, deadline())?;
    let progress = restore.progress(deadline())?;
    if progress.done {
        if let Some(log)=&legacy.log { log.verify()?; }
        return restore.finish(progress.cursor, deadline()).map_err(Into::into);
    }
    let mut cursor = progress.cursor;
    let mut position = if cursor.is_empty() {
        let schema = legacy.tables.iter().map(|t| t.sql.as_str()).collect::<Vec<_>>().join(";\n");
        let position = Position::Rows { table: 0, after: None };
        let next = postcard::to_allocvec(&position)?;
        restore.apply_page(RestorePage { expected_cursor: cursor, next_cursor: next.clone(), schema: (!schema.is_empty()).then_some(schema), statements: Vec::new() }, deadline())?;
        cursor = next;
        position
    } else { postcard::from_bytes(&cursor)? };
    let mut pages = 0usize;
    loop {
        match position {
            Position::Rows { table, after } => {
                let (next_position, statements) = if let Some(source) = legacy.tables.get(table) {
                    if let Some(page) = legacy.page(source, after.as_ref())? {
                        let mut columns = source.columns.iter().map(|c| quote(c)).collect::<Vec<_>>();
                        if source.rowid { columns.insert(0, quote(source.keys.first().context("legacy rowid key missing")?)); }
                        let sql = format!("INSERT INTO {}({}) VALUES ({})", quote(&source.name), columns.join(","), (1..=columns.len()).map(|n| format!("?{n}")).collect::<Vec<_>>().join(","));
                        let statements = page.rows.into_iter().map(|params| Statement { sql:sql.clone(), params }).collect();
                        (Position::Rows { table, after: Some(page.next) }, statements)
                    } else {
                        (Position::Rows { table: table.checked_add(1).context("legacy table index overflow")?, after: None }, Vec::new())
                    }
                } else { (Position::Objects, Vec::new()) };
                let next = postcard::to_allocvec(&next_position)?;
                let had_rows = !statements.is_empty();
                restore.apply_page(RestorePage { expected_cursor: cursor, next_cursor: next.clone(), schema: None, statements }, deadline())?;
                cursor = next;
                position = next_position;
                if had_rows {
                    pages = pages.saturating_add(1);
                    anyhow::ensure!(stop_after.is_none_or(|limit| pages < limit), "test interruption after durable import page");
                }
            }
            Position::Objects => {
                let schema = legacy.objects.iter().filter(|o| o.kind != "table").map(|o| o.sql.as_str()).collect::<Vec<_>>().join(";\n");
                let sequence: Vec<(String, i64)> = if legacy.connection.query_row("SELECT count(*) FROM sqlite_schema WHERE name='sqlite_sequence'", [], |r| r.get::<_, i64>(0))? > 0 {
                    legacy.connection.prepare("SELECT name,seq FROM sqlite_sequence")?.query_map([], |r| Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
                } else { Vec::new() };
                let mut statements = Vec::new();
                // AUTOINCREMENT stays above deleted rows too, not merely the imported maximum id.
                for (table, sequence) in sequence {
                    if legacy.objects.iter().any(|o| o.kind == "table" && o.name == table) {
                        statements.push(Statement { sql:"DELETE FROM sqlite_sequence WHERE name=?1".into(), params:vec![Value::Text(table.clone())] });
                        statements.push(Statement { sql:"INSERT INTO sqlite_sequence(name,seq) VALUES (?1,?2)".into(), params:vec![Value::Text(table),Value::Integer(sequence)] });
                    }
                }
                let next = postcard::to_allocvec(&Position::Finished)?;
                restore.apply_page(RestorePage { expected_cursor: cursor, next_cursor: next.clone(), schema: (!schema.is_empty()).then_some(schema), statements }, deadline())?;
                cursor = next;
                position = Position::Finished;
            }
            Position::Finished => {
                if let Some(log)=&legacy.log { log.verify()?; }
                return restore.finish(cursor, deadline()).map_err(Into::into);
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
struct LedgerPosition { table: usize, after: Option<Cursor> }

/// U05/U07: an already populated host receives the runner ledger's Records-shaped rows and
/// trust audit trail. Conflicting immutable records refuse the merge rather than overwrite a
/// result. Each batch and its source/cursor fence share the host writer transaction.
pub(super) fn import_work(database: &Database, path: &Path, host_key:&str) -> anyhow::Result<()> {
    let parent=path.parent().context("runner ledger source has no parent")?;
    let parent=if parent.is_dir() { parent.canonicalize()? } else { std::path::absolute(parent)? };
    let source_path=parent.join(path.file_name().context("runner ledger source has no filename")?);
    let source=format!("runner-ledger:{}",serde_json::to_string(&(host_key,source_path.to_string_lossy()))?);
    let key = source.clone();
    let previous = database.call(deadline(), move |c| {
        use crate::sql::OptionalExtension as _;
        Ok(c.query_row("SELECT identity,cursor,done FROM runner_work_imports WHERE source=?1", [key], |r| Ok((r.get::<_, String>(0)?,r.get::<_, Vec<u8>>(1)?,r.get::<_, bool>(2)?))).optional()?)
    })?;
    if previous.as_ref().is_some_and(|(_,_,done)| *done) { return Ok(()); }
    if !path.is_file() {
        anyhow::ensure!(previous.is_none(),"runner ledger import is incomplete and its source {} is unavailable; restore the pinned source before accepting work",path.display());
        return Ok(());
    }
    let legacy = Legacy::open(path)?;
    let identity = legacy.identity()?;
    let mut cursor = if let Some((known,cursor,_)) = previous {
        anyhow::ensure!(identity == known, "the runner ledger changed since import began; restore its original snapshot before resuming");
        cursor
    } else {
        let (source,identity) = (source.clone(),identity.clone());
        database.call(deadline(), move |c| {
            c.execute("INSERT INTO runner_work_imports(source,identity,cursor,done) VALUES (?1,?2,?3,0)",crate::sql::params![source,identity,Vec::<u8>::new()])?;
            Ok(())
        })?;
        Vec::new()
    };
    let tables: Vec<_> = legacy.tables.iter().filter(|t| ["work_records","audit2","audit"].contains(&t.name.as_str())).collect();
    let mut position: LedgerPosition = if cursor.is_empty() { LedgerPosition { table:0,after:None } } else { postcard::from_bytes(&cursor)? };
    loop {
        let Some(table) = tables.get(position.table) else {
            if let Some(log)=&legacy.log { log.verify()?; }
            let (source,identity) = (source.clone(),identity.clone());
            database.call(deadline(), move |c| {
                anyhow::ensure!(c.execute("UPDATE runner_work_imports SET done=1 WHERE source=?1 AND identity=?2 AND cursor=?3 AND done=0",crate::sql::params![source,identity,cursor])? == 1,"runner ledger completion cursor changed");
                Ok(())
            })?;
            return Ok(());
        };
        let page = legacy.page(table,position.after.as_ref())?;
        let next_position = if let Some(page) = &page { LedgerPosition { table:position.table,after:Some(page.next.clone()) } } else { LedgerPosition { table:position.table.checked_add(1).context("ledger table index overflow")?,after:None } };
        let next = postcard::to_allocvec(&next_position)?;
        let (source,identity,table,next_cursor) = (source.clone(),identity.clone(),(**table).clone(),next.clone());
        database.call(deadline(), move |c| {
            let state: (String,Vec<u8>,bool) = c.query_row("SELECT identity,cursor,done FROM runner_work_imports WHERE source=?1",[&source],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            anyhow::ensure!(state.0 == identity && state.1 == cursor && !state.2,"runner ledger source or cursor changed");
            if let Some(page) = page {
                for row in page.rows {
                    let values: Vec<_> = row.into_iter().skip(usize::from(table.rowid)).collect();
                    if table.name == "work_records" {
                        let at = |name: &str| -> anyhow::Result<Value> { let n=table.columns.iter().position(|c| c==name).context("legacy work column missing")?; values.get(n).cloned().context("legacy work value missing") };
                        let (family,key,value,time)=(at("family")?,at("key")?,at("value")?,at("at")?);
                        c.execute("INSERT INTO work_records(family,key,value,at) VALUES(?1,?2,?3,?4) ON CONFLICT(family,key) DO NOTHING",crate::sql::params![family,key,value,time])?;
                        let kept: Value = c.query_row("SELECT value FROM work_records WHERE family=?1 AND key=?2",crate::sql::params![family,key],|r| r.get(0))?;
                        anyhow::ensure!(kept==value,"runner work import conflicts with an existing immutable record");
                        c.execute("UPDATE work_records SET at=max(at,?3) WHERE family=?1 AND key=?2",crate::sql::params![family,key,time])?;
                    } else {
                        // Audit identifiers were local to the retired ledger; allocate host ids,
                        // retaining every original field while the import cursor prevents repeats.
                        let columns: Vec<_> = table.columns.iter().enumerate().filter(|(_,name)| name.as_str()!="id").collect();
                        let sql=format!("INSERT INTO {}({}) VALUES({})",quote(&table.name),columns.iter().map(|(_,name)| quote(name)).collect::<Vec<_>>().join(","),(1..=columns.len()).map(|n|format!("?{n}")).collect::<Vec<_>>().join(","));
                        let values=columns.into_iter().map(|(n,_)|values.get(n).cloned().context("legacy audit value missing")).collect::<anyhow::Result<Vec<_>>>()?;
                        c.execute(&sql,values)?;
                    }
                }
            }
            c.execute("UPDATE runner_work_imports SET cursor=?2 WHERE source=?1",crate::sql::params![source,next_cursor])?;
            Ok(())
        })?;
        cursor = next;
        position = next_position;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn u05_legacy_import_resumes_preserving_rowids_composite_keys_and_sequences() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("kiln.db");
        let old = Connection::open(&source)?;
        old.execute_batch("CREATE TABLE a(k TEXT PRIMARY KEY,v BLOB); CREATE TABLE b(x TEXT,y INTEGER,v TEXT,PRIMARY KEY(x,y)) WITHOUT ROWID; CREATE TABLE n(id INTEGER PRIMARY KEY AUTOINCREMENT,v TEXT); INSERT INTO n VALUES(1,'keep'),(90,'gone'); DELETE FROM n WHERE id=90;")?;
        for n in -2..260i64 { old.execute("INSERT INTO a(rowid,k,v) VALUES(?1,?2,?3)", rusqlite::params![n,format!("k{n}"),vec![1u8,2,3]])?; }
        old.execute("INSERT INTO b VALUES('same',-2,'first'),('same',7,'last')", [])?;
        drop(old);
        let destination = dir.path().join("amphora.db");
        assert!(import_pages(&destination, &source, Some(1)).is_err());
        assert!(Database::open(&destination, deadline()).is_err(), "an incomplete source cannot serve live queries");
        let db = import(&destination, &source, None)?;
        db.call(deadline(), |c| {
            assert_eq!(c.query_row("SELECT count(*) FROM a", [], |r| r.get::<_, i64>(0))?,262);
            assert_eq!(c.query_row("SELECT rowid FROM a WHERE k='k-2'", [], |r| r.get::<_, i64>(0))?,-2);
            assert_eq!(c.query_row("SELECT count(*) FROM b", [], |r| r.get::<_, i64>(0))?,2);
            assert_eq!(c.query_row("SELECT id FROM n WHERE v='keep'", [], |r| r.get::<_, i64>(0))?,1);
            c.execute("INSERT INTO n(v) VALUES('next')", [])?;
            assert_eq!(c.last_insert_rowid(),91);
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn u05_a_changed_legacy_source_cannot_resume_into_another_snapshot() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("kiln.db");
        let old = Connection::open(&source)?;
        old.execute_batch("CREATE TABLE a(k INTEGER PRIMARY KEY,v TEXT); INSERT INTO a VALUES(1,'before');")?;
        drop(old);
        let destination = dir.path().join("amphora.db");
        assert!(import_pages(&destination, &source, Some(1)).is_err());
        Connection::open(&source)?.execute("UPDATE a SET v='after'", [])?;
        assert!(import(&destination, &source, None).is_err(), "a different snapshot cannot advance the old cursor");
        #[cfg(unix)]
        {
            let fresh = tempfile::tempdir()?;
            let empty = fresh.path().join("kiln.db");
            let fresh_store = crate::store::Store::open(&empty)?;
            assert!(empty.is_dir(), "fresh canonical acceptance also fences old CREATE-on-open");
            drop(fresh_store);
            assert!(crate::store::Store::cutover_offline(Path::new("")).is_err());
            // Orphan sidecars/log are legacy inputs too. Fresh startup must not chmod/link
            // them or create a pending transition without the explicit offline authority.
            for role in ["kiln.db-wal", "kiln.db-shm", "cluster/cluster.log", "kiln.db-journal"] {
                let orphan = tempfile::tempdir()?;
                let input = orphan.path().join(role);
                std::fs::create_dir_all(input.parent().context("orphan parent")?)?;
                std::fs::write(&input, b"preserve orphan source")?;
                let permissions = std::fs::metadata(&input)?.permissions();
                assert!(crate::store::Store::open(&orphan.path().join("kiln.db")).is_err());
                assert_eq!(std::fs::read(&input)?, b"preserve orphan source");
                assert_eq!(std::fs::metadata(&input)?.permissions(), permissions);
                assert!(!orphan.path().join("kiln.db.amphora-cutover").exists());
            }
            // Newly created data ancestors need their own publication proof. A retry must
            // not assume an existing newparent/data survived the earlier failed barrier.
            let ancestry = tempfile::tempdir()?;
            let grandparent = ancestry.path().join("newparent");
            let data = grandparent.join("data");
            let nested = data.join("kiln.db");
            let mut refuse_ancestor = |path: &Path| -> anyhow::Result<()> {
                if path == grandparent { anyhow::bail!("injected new ancestor sync failure"); }
                std::fs::File::open(path)?.sync_all()?;
                Ok(())
            };
            for _ in 0..2 {
                assert!(super::super::cutover::with_sync(&nested, false, &mut refuse_ancestor).is_err());
                assert!(data.is_dir());
                assert!(!nested.exists());
                assert!(!data.join("kiln.db.amphora-cutover").exists());
            }
            crate::store::Store::open(&nested)?;
            // Unknown final parent-sync outcome leaves a readable directory. Existence alone
            // must not admit a cached or new owner: retry the actual barrier before acceptance.
            let unsynced = tempfile::tempdir()?;
            let final_path = unsynced.path().join("kiln.db");
            let mut refuse_final = |path: &Path| -> anyhow::Result<()> {
                if path == unsynced.path() && final_path.is_dir() { anyhow::bail!("injected final parent sync failure"); }
                std::fs::File::open(path)?.sync_all()?;
                Ok(())
            };
            assert!(super::super::cutover::with_sync(&final_path, true, &mut refuse_final).is_err());
            assert!(final_path.is_dir());
            assert!(super::super::cutover::with_sync(&final_path, false, &mut refuse_final).is_err(), "existing fence cannot bypass a failed publication barrier");
            crate::store::Store::open(&final_path)?;
            let offline = tempfile::tempdir()?;
            let original = offline.path().join("kiln.db");
            let old = Connection::open(&original)?;
            old.execute_batch("CREATE TABLE offline_value(v TEXT); INSERT INTO offline_value VALUES('preserved');")?;
            drop(old);
            let before = std::fs::read(&original)?;
            assert!(crate::store::Store::open(&original).is_err(), "ordinary startup cannot silently import an unfenced legacy writer");
            assert_eq!(std::fs::read(&original)?, before);
            let rollback = offline.path().join("kiln.db-journal");
            std::fs::write(&rollback, b"rollback source whose hotness must not be guessed")?;
            let permissions = std::fs::metadata(&original)?.permissions();
            assert!(crate::store::Store::cutover_offline(offline.path()).is_err());
            assert_eq!(std::fs::read(&original)?, before);
            assert_eq!(std::fs::metadata(&original)?.permissions(), permissions);
            assert_eq!(std::fs::read(&rollback)?, b"rollback source whose hotness must not be guessed");
            assert!(!offline.path().join("kiln.db.amphora-cutover").exists());
            std::fs::remove_file(&rollback)?; // This test-owned marker was never a SQL journal.
            crate::store::Store::cutover_offline(offline.path())?;
            assert!(original.is_dir());
            assert!(Connection::open(&original).is_err(), "actual old SQLite open is blocked, including old replica open");
            let replacement = offline.path().join("kiln.copy");
            std::fs::write(&replacement, b"old replica replacement")?;
            assert!(std::fs::rename(&replacement, &original).is_err(), "old replica rename cannot overwrite the directory fence");
            assert_eq!(std::fs::read(original.join("source.sqlite"))?, before);
            let store = crate::store::Store::open(&original)?;
            store.now(|c| { assert_eq!(c.query_row("SELECT v FROM offline_value", [], |r| r.get::<_,String>(0))?, "preserved"); Ok(()) })?;
            // A cached canonical owner cannot bypass a reintroduced regular old pathname.
            let held = offline.path().join("held-fence");
            std::fs::rename(&original, &held)?;
            std::fs::write(&original, b"unfenced old pathname")?;
            assert!(crate::store::Store::open(&original).is_err());
            assert_eq!(std::fs::read(&original)?, b"unfenced old pathname");
            std::fs::remove_file(&original)?;
            std::fs::rename(&held, &original)?;
            // This is the actual durable state after source unlink and before fence publication.
            // Even an already-live process registry cannot turn it into a fresh activation.
            let pending = offline.path().join("kiln.db.amphora-cutover");
            std::fs::rename(&original, &pending)?;
            assert!(crate::store::Store::open(&original).is_err());
            // Reconstruct the earlier crash point: durable inventory with original source
            // link still present. A failed/retried preservation barrier cannot unlink it.
            std::fs::hard_link(pending.join("source.sqlite"), &original)?;
            let mut refuse_pending = |path: &Path| -> anyhow::Result<()> {
                if path == pending { anyhow::bail!("injected pending directory sync failure"); }
                std::fs::File::open(path)?.sync_all()?;
                Ok(())
            };
            for _ in 0..2 {
                assert!(super::super::cutover::with_sync(&original, true, &mut refuse_pending).is_err());
                assert!(original.is_file(), "preservation barriers must complete before original unlink");
                assert_eq!(std::fs::read(&original)?, before);
            }
            crate::store::Store::cutover_offline(offline.path())?;
            crate::store::Store::open(&original)?;
            assert_eq!(std::fs::read(original.join("source.sqlite"))?, before);
            // Reproduce the old-upgrade gap: a completed restore followed by an old accepted
            // write is divergence, not permission to silently keep serving the earlier copy.
            let diverged = tempfile::tempdir()?;
            let source = diverged.path().join("kiln.db");
            let old = Connection::open(&source)?;
            old.execute_batch("CREATE TABLE value(v TEXT); INSERT INTO value VALUES('before');")?;
            drop(old);
            drop(import(&diverged.path().join("amphora.db"), &source, None)?);
            Connection::open(&source)?.execute("UPDATE value SET v='accepted-by-old'", [])?;
            assert!(crate::store::Store::cutover_offline(diverged.path()).is_err());
            assert!(crate::store::Store::open(&source).is_err(), "completed import must not discard old accepted writes");
            // Copy a tiny committed WAL snapshot only to construct a stopped test fixture;
            // production cutover uses same-filesystem preservation, never this copy path.
            let writer = tempfile::tempdir()?;
            let live = writer.path().join("kiln.db");
            let old = Connection::open(&live)?;
            old.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE wal_only(v TEXT); INSERT INTO wal_only VALUES('in-wal');")?;
            let snapshot = tempfile::tempdir()?;
            let mut preserved = Vec::new();
            for suffix in ["", "-wal", "-shm"] {
                let from = writer.path().join(format!("kiln.db{suffix}"));
                if from.is_file() {
                    let to = snapshot.path().join(format!("kiln.db{suffix}"));
                    std::fs::copy(&from, &to)?;
                    preserved.push((suffix, std::fs::read(to)?));
                }
            }
            drop(old);
            crate::store::Store::cutover_offline(snapshot.path())?;
            let source = snapshot.path().join("kiln.db");
            for (suffix, bytes) in preserved { assert_eq!(std::fs::read(source.join(format!("source.sqlite{suffix}")))?, bytes, "preserved WAL snapshot{suffix}"); }
            let store = crate::store::Store::open(&source)?;
            store.now(|c| { assert_eq!(c.query_row("SELECT v FROM wal_only", [], |r|r.get::<_,String>(0))?, "in-wal"); Ok(()) })?;
        }
        Ok(())
    }
}
