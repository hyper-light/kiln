//! What a write did to kiln.db: the rows it touched, as SQLite's pre-update hook names them, and
//! each one's state once the write is done. That net change is what an entry of the cluster group
//! carries (docs/p2p.md, P1), applied on every replica as the same rows with the same rowids, so a
//! replica is the same database, not just an equal one: kiln uses rowids as identifiers (a run's is
//! its GitHub run id; a job's orders its legs).
//!
//! The hook only notes which rows a write touched (a table and a rowid; a table without rowids, its
//! primary key). What a row ended as is read when the write is done, from the write's own
//! savepoint: so a part it took back (a savepoint of its own, a statement that failed half way)
//! leaves nothing, and a write that failed after writing keeps what it kept before, as without the
//! group. Applying is replaying those rows: deleted ones go, the rest are written whole. The
//! connection that applies has foreign keys off: the write's own cascades are among its rows
//! already, and must not fire again.

#![allow(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::Arc;

use anyhow::{Context as _, bail};
use parking_lot::Mutex;
use rusqlite::types::{ToSqlOutput, ValueRef};
use rusqlite::{Connection, ffi};
use serde::{Deserialize, Serialize};

/// A column's value as SQLite holds it (text as its bytes: SQLite doesn't promise UTF-8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Val {
    Null,
    Int(i64),
    Real(f64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

impl rusqlite::ToSql for Val {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(match self {
            Val::Null => ValueRef::Null,
            Val::Int(i) => ValueRef::Integer(*i),
            Val::Real(f) => ValueRef::Real(*f),
            Val::Text(b) => ValueRef::Text(b),
            Val::Blob(b) => ValueRef::Blob(b),
        }))
    }
}

impl Val {
    fn of(v: ValueRef<'_>) -> Val {
        match v {
            ValueRef::Null => Val::Null,
            ValueRef::Integer(i) => Val::Int(i),
            ValueRef::Real(f) => Val::Real(f),
            ValueRef::Text(b) => Val::Text(b.to_vec()),
            ValueRef::Blob(b) => Val::Blob(b.to_vec()),
        }
    }
}

/// Which row: its rowid, or for a table without rowids its primary key's values.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Key {
    Rowid(i64),
    Pk(Vec<Bits>),
}

/// A value that hashes (a primary key's part): reals by their bits.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Bits {
    Null,
    Int(i64),
    Real(u64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

impl From<&Val> for Bits {
    fn from(v: &Val) -> Bits {
        match v {
            Val::Null => Bits::Null,
            Val::Int(i) => Bits::Int(*i),
            Val::Real(f) => Bits::Real(f.to_bits()),
            Val::Text(b) => Bits::Text(b.clone()),
            Val::Blob(b) => Bits::Blob(b.clone()),
        }
    }
}

impl From<&Bits> for Val {
    fn from(b: &Bits) -> Val {
        match b {
            Bits::Null => Val::Null,
            Bits::Int(i) => Val::Int(*i),
            Bits::Real(f) => Val::Real(f64::from_bits(*f)),
            Bits::Text(t) => Val::Text(t.clone()),
            Bits::Blob(b) => Val::Blob(b.clone()),
        }
    }
}

/// A row as a write left it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Row {
    /// Gone (or never kept: a row it made and took back).
    Gone { table: String, key: Key },
    /// Its every column (stored ones, in the table's order); a rowid table's rowid too.
    Kept { table: String, rowid: Option<i64>, values: Vec<Val> },
}

/// What one write changed: its rows, the gone ones first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub rows: Vec<Row>,
}

impl Change {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        postcard::to_allocvec(self).context("encoding a change")
    }

    pub fn decode(bytes: &[u8]) -> anyhow::Result<Change> {
        postcard::from_bytes(bytes).context("decoding a change")
    }
}

/// A table as a change needs it: its stored columns, and how its rows are found.
#[derive(Debug, Clone)]
struct Table {
    /// Stored columns, quoted, in order.
    columns: Vec<String>,
    /// A table without rowids: its primary key's columns (indexes into `columns`), in key order.
    pk: Option<Vec<usize>>,
    /// The column that is the rowid (`INTEGER PRIMARY KEY`), if one is.
    alias: Option<usize>,
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The tables of `conn`'s main database as changes need them, and those without rowids by name.
fn tables(conn: &Connection) -> anyhow::Result<HashMap<String, Table>> {
    let mut out = HashMap::new();
    let names: Vec<(String, String)> = {
        let mut st = conn.prepare("SELECT name, COALESCE(sql, '') FROM main.sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
    };
    for (name, sql) in names {
        if sql.to_ascii_uppercase().starts_with("CREATE VIRTUAL TABLE") {
            continue;
        }
        let rowless = sql.to_ascii_uppercase().replace(char::is_whitespace, "").contains("WITHOUTROWID");
        // cid, name, type, notnull, dflt_value, pk, hidden
        let mut st = conn.prepare(&format!("PRAGMA main.table_xinfo({})", quote(&name)))?;
        let cols: Vec<(String, String, i64, i64)> = st.query_map([], |r| Ok((r.get(1)?, r.get::<_, String>(2)?, r.get(5)?, r.get(6)?)))?.collect::<Result<_, _>>()?;
        let stored: Vec<&(String, String, i64, i64)> = cols.iter().filter(|c| c.3 == 0).collect();
        let pks: Vec<(usize, i64)> = stored.iter().enumerate().filter(|(_, c)| c.2 > 0).map(|(i, c)| (i, c.2)).collect();
        let alias = match pks.as_slice() {
            [(i, _)] if !rowless && stored.get(*i).is_some_and(|c| c.1.eq_ignore_ascii_case("INTEGER")) => Some(*i),
            _ => None,
        };
        let pk = rowless.then(|| {
            let mut p = pks.clone();
            p.sort_by_key(|(_, order)| *order);
            p.into_iter().map(|(i, _)| i).collect()
        });
        out.insert(name, Table { columns: stored.iter().map(|c| quote(&c.0)).collect(), pk, alias });
    }
    Ok(out)
}

/// What the pre-update hook writes into.
#[derive(Default)]
pub struct Capture {
    on: bool,
    /// Touched rows, first touch first, and whether each was seen.
    touched: Vec<(String, Key)>,
    seen: HashSet<(String, Key)>,
    /// Tables without rowids: their primary key's column indexes.
    rowless: HashMap<String, Vec<usize>>,
    /// Tables this node keeps for itself: a write that touches them goes past the group.
    local: HashSet<String>,
    /// Tables a write touched that it must not (this node's own ones, through the group).
    strays: HashSet<String>,
    /// A change the hook couldn't read.
    error: Option<String>,
}

impl Capture {
    fn touch(&mut self, table: &str, key: Key) {
        let k = (table.to_owned(), key);
        if self.seen.insert(k.clone()) {
            self.touched.push(k);
        }
    }
}

/// The capture a connection's hook writes into, and the tables it knows.
pub struct Watch {
    capture: Arc<Mutex<Capture>>,
    tables: Mutex<HashMap<String, Table>>,
}

/// Each row's key from a pre-update call: the rowid, or a rowless table's primary key read with
/// `read` (`sqlite3_preupdate_old` or `_new`).
unsafe fn pk_of(db: *mut ffi::sqlite3, pk: &[usize], read: unsafe extern "C" fn(*mut ffi::sqlite3, c_int, *mut *mut ffi::sqlite3_value) -> c_int) -> Result<Key, String> {
    let mut parts = Vec::with_capacity(pk.len());
    for &i in pk {
        let mut v: *mut ffi::sqlite3_value = std::ptr::null_mut();
        let rc = unsafe { read(db, c_int::try_from(i).unwrap_or(c_int::MAX), &mut v) };
        if rc != ffi::SQLITE_OK || v.is_null() {
            return Err(format!("a primary key column couldn't be read ({rc})"));
        }
        parts.push(Bits::from(&unsafe { value_of(v) }));
    }
    Ok(Key::Pk(parts))
}

/// A value the hook was handed.
unsafe fn value_of(v: *mut ffi::sqlite3_value) -> Val {
    unsafe {
        match ffi::sqlite3_value_type(v) {
            ffi::SQLITE_INTEGER => Val::Int(ffi::sqlite3_value_int64(v)),
            ffi::SQLITE_FLOAT => Val::Real(ffi::sqlite3_value_double(v)),
            ffi::SQLITE_TEXT => {
                let p = ffi::sqlite3_value_text(v);
                let n = usize::try_from(ffi::sqlite3_value_bytes(v)).unwrap_or(0);
                Val::Text(if p.is_null() { Vec::new() } else { std::slice::from_raw_parts(p, n).to_vec() })
            }
            ffi::SQLITE_BLOB => {
                let p = ffi::sqlite3_value_blob(v).cast::<u8>();
                let n = usize::try_from(ffi::sqlite3_value_bytes(v)).unwrap_or(0);
                Val::Blob(if p.is_null() { Vec::new() } else { std::slice::from_raw_parts(p, n).to_vec() })
            }
            _ => Val::Null,
        }
    }
}

unsafe extern "C" fn hook(ctx: *mut c_void, db: *mut ffi::sqlite3, op: c_int, z_db: *const c_char, z_table: *const c_char, key1: i64, key2: i64) {
    // SAFETY: `ctx` is the `Mutex<Capture>` the `Watch` that installed this hook holds, alive as long
    // as the connection (the store drops the connection first); the hook runs on the thread running
    // the statement, under the store's lock.
    let capture = unsafe { &*ctx.cast::<Mutex<Capture>>() };
    let mut c = capture.lock();
    if !c.on || z_db.is_null() || z_table.is_null() {
        return;
    }
    let (Ok(dbname), Ok(table)) = (unsafe { CStr::from_ptr(z_db) }.to_str(), unsafe { CStr::from_ptr(z_table) }.to_str()) else {
        c.error = Some("a change to a table whose name isn't UTF-8".into());
        return;
    };
    if dbname != "main" || table.starts_with("sqlite_") {
        return;
    }
    if c.local.contains(table) {
        c.strays.insert(table.to_owned());
        return;
    }
    match c.rowless.get(table).cloned() {
        None => {
            c.touch(table, Key::Rowid(key1));
            if op == ffi::SQLITE_UPDATE && key2 != key1 {
                c.touch(table, Key::Rowid(key2));
            }
        }
        Some(pk) => {
            let old = (op != ffi::SQLITE_INSERT).then(|| unsafe { pk_of(db, &pk, ffi::sqlite3_preupdate_old) });
            let new = (op != ffi::SQLITE_DELETE).then(|| unsafe { pk_of(db, &pk, ffi::sqlite3_preupdate_new) });
            for k in [old, new].into_iter().flatten() {
                match k {
                    Ok(k) => c.touch(table, k),
                    Err(e) => c.error = Some(format!("{table}: {e}")),
                }
            }
        }
    }
}

impl Watch {
    /// Puts a hook on `conn` that notes the rows its writes touch while a capture is on; `local`
    /// are tables this node keeps for itself.
    pub fn install(conn: &Connection, local: &[&str]) -> anyhow::Result<Watch> {
        let tables = tables(conn)?;
        let rowless = tables.iter().filter_map(|(n, t)| t.pk.clone().map(|pk| (n.clone(), pk))).collect();
        let capture = Arc::new(Mutex::new(Capture { rowless, local: local.iter().map(|t| (*t).to_owned()).collect(), ..Capture::default() }));
        // SAFETY: the hook reads `capture` through this pointer for as long as it's installed; the
        // `Watch` keeps the `Arc` and must outlive the connection's statements (the store holds both,
        // the connection dropped first).
        unsafe {
            ffi::sqlite3_preupdate_hook(conn.handle(), Some(hook), Arc::as_ptr(&capture).cast_mut().cast::<c_void>());
        }
        Ok(Watch { capture, tables: Mutex::new(tables) })
    }

    /// Starts noting what writes touch.
    pub fn begin(&self) {
        let mut c = self.capture.lock();
        c.on = true;
        c.touched.clear();
        c.seen.clear();
        c.strays.clear();
        c.error = None;
    }

    /// Stops noting, and reads what the touched rows are now, on `conn` (still inside the write's
    /// savepoint): the write's change.
    pub fn end(&self, conn: &Connection) -> anyhow::Result<Change> {
        let (touched, strays, error) = {
            let mut c = self.capture.lock();
            c.on = false;
            c.seen.clear();
            (std::mem::take(&mut c.touched), std::mem::take(&mut c.strays), c.error.take())
        };
        if let Some(e) = error {
            bail!("a change couldn't be read: {e}");
        }
        if !strays.is_empty() {
            bail!("a write through the cluster group touched this node's own tables: {}", strays.into_iter().collect::<Vec<_>>().join(", "));
        }
        let mut gone = Vec::new();
        let mut kept = Vec::new();
        let mut tables = self.tables.lock();
        for (table, key) in touched {
            if !tables.contains_key(&table) {
                // A table made after the hook learnt them: learn again.
                *tables = self::tables(conn)?;
            }
            let Some(t) = tables.get(&table) else { bail!("a change to table {table}, which isn't one") };
            match read(conn, &table, t, &key)? {
                Some((rowid, values)) => kept.push(Row::Kept { table, rowid, values }),
                None => gone.push(Row::Gone { table, key }),
            }
        }
        gone.extend(kept);
        Ok(Change { rows: gone })
    }
}

/// A row's rowid (for a rowid table) and stored values, if it's there.
fn read(conn: &Connection, name: &str, t: &Table, key: &Key) -> anyhow::Result<Option<(Option<i64>, Vec<Val>)>> {
    let cols = t.columns.join(", ");
    let n = t.columns.len();
    match (key, &t.pk) {
        (Key::Rowid(r), None) => {
            let mut st = conn.prepare_cached(&format!("SELECT rowid, {cols} FROM main.{} WHERE rowid = ?1", quote(name)))?;
            let mut rows = st.query([r])?;
            let Some(row) = rows.next()? else { return Ok(None) };
            let values = (1..=n).map(|i| row.get_ref(i).map(Val::of)).collect::<Result<Vec<_>, _>>()?;
            Ok(Some((Some(row.get(0)?), values)))
        }
        (Key::Pk(parts), Some(pk)) => {
            let filter = pk.iter().enumerate().map(|(j, &i)| format!("{} IS ?{}", t.columns.get(i).map(String::as_str).unwrap_or("NULL"), j + 1)).collect::<Vec<_>>().join(" AND ");
            let mut st = conn.prepare_cached(&format!("SELECT {cols} FROM main.{} WHERE {filter}", quote(name)))?;
            let args: Vec<Val> = parts.iter().map(Val::from).collect();
            let mut rows = st.query(rusqlite::params_from_iter(args.iter()))?;
            let Some(row) = rows.next()? else { return Ok(None) };
            let values = (0..n).map(|i| row.get_ref(i).map(Val::of)).collect::<Result<Vec<_>, _>>()?;
            Ok(Some((None, values)))
        }
        _ => bail!("table {name} is found by {} but this change keys it otherwise", if t.pk.is_some() { "its primary key" } else { "rowid" }),
    }
}

/// Applies `change` on `conn` (inside the caller's transaction; foreign keys off): its gone rows
/// go, its kept rows are written whole, with their rowids.
pub fn apply(conn: &Connection, change: &Change, known: &mut HashMap<String, TableInfo>) -> anyhow::Result<()> {
    for row in &change.rows {
        let table = match row {
            Row::Gone { table, .. } | Row::Kept { table, .. } => table,
        };
        if !known.contains_key(table) {
            known.clear();
            known.extend(tables(conn)?.into_iter().map(|(n, t)| (n, TableInfo(t))));
        }
        let Some(TableInfo(t)) = known.get(table) else { bail!("a change to table {table}, which this database hasn't") };
        match row {
            Row::Gone { key: Key::Rowid(r), .. } => {
                conn.prepare_cached(&format!("DELETE FROM main.{} WHERE rowid = ?1", quote(table)))?.execute([r])?;
            }
            Row::Gone { key: Key::Pk(parts), .. } => {
                let Some(pk) = &t.pk else { bail!("table {table} has rowids; this change keys it by its primary key") };
                let filter = pk.iter().enumerate().map(|(j, &i)| format!("{} IS ?{}", t.columns.get(i).map(String::as_str).unwrap_or("NULL"), j + 1)).collect::<Vec<_>>().join(" AND ");
                let args: Vec<Val> = parts.iter().map(Val::from).collect();
                conn.prepare_cached(&format!("DELETE FROM main.{} WHERE {filter}", quote(table)))?.execute(rusqlite::params_from_iter(args.iter()))?;
            }
            Row::Kept { rowid, values, .. } => {
                if values.len() != t.columns.len() {
                    bail!("table {table} has {} columns here; this change has {}", t.columns.len(), values.len());
                }
                let cols = t.columns.join(", ");
                match (rowid, t.alias, &t.pk) {
                    (None, None, None) => bail!("a row of rowid table {table} without its rowid"),
                    // The rowid is a column already, or there is none.
                    (_, Some(_), _) | (_, _, Some(_)) => {
                        let marks = (1..=values.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ");
                        conn.prepare_cached(&format!("INSERT OR REPLACE INTO main.{} ({cols}) VALUES ({marks})", quote(table)))?.execute(rusqlite::params_from_iter(values.iter()))?;
                    }
                    (Some(r), None, None) => {
                        let marks = (2..=values.len() + 1).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ");
                        let mut args: Vec<Val> = Vec::with_capacity(values.len() + 1);
                        args.push(Val::Int(*r));
                        args.extend(values.iter().cloned());
                        conn.prepare_cached(&format!("INSERT OR REPLACE INTO main.{} (rowid, {cols}) VALUES (?1, {marks})", quote(table)))?.execute(rusqlite::params_from_iter(args.iter()))?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// What [`apply`] keeps of the tables between changes.
#[derive(Debug, Clone)]
pub struct TableInfo(Table);

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row of every table of `conn`, rowids included, in order: two databases with equal
    /// dumps are the same database.
    fn dump(conn: &Connection) -> anyhow::Result<Vec<String>> {
        let mut out = Vec::new();
        for (name, t) in {
            let mut v: Vec<(String, Table)> = tables(conn)?.into_iter().collect();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            v
        } {
            let cols = t.columns.join(", ");
            let sql = if t.pk.is_some() { format!("SELECT {cols} FROM {} ORDER BY {cols}", quote(&name)) } else { format!("SELECT rowid, {cols} FROM {} ORDER BY rowid", quote(&name)) };
            let mut st = conn.prepare(&sql)?;
            let n = st.column_count();
            let mut rows = st.query([])?;
            while let Some(r) = rows.next()? {
                let vals = (0..n).map(|i| r.get_ref(i).map(|v| format!("{:?}", Val::of(v)))).collect::<Result<Vec<_>, _>>()?;
                out.push(format!("{name}: {}", vals.join(" | ")));
            }
        }
        let seq: Vec<String> = match conn.prepare("SELECT name, seq FROM sqlite_sequence ORDER BY name") {
            Ok(mut st) => st.query_map([], |r| Ok(format!("seq {}={}", r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.collect::<Result<_, _>>()?,
            Err(_) => Vec::new(),
        };
        out.extend(seq);
        Ok(out)
    }

    const SCHEMA: &str = "PRAGMA foreign_keys = ON;
        CREATE TABLE runs (id TEXT PRIMARY KEY, n INTEGER, note TEXT);
        CREATE TABLE jobs (id TEXT PRIMARY KEY, run TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE, name TEXT, score REAL, data BLOB);
        CREATE TABLE audit (id INTEGER PRIMARY KEY AUTOINCREMENT, what TEXT);
        CREATE TABLE subjects (attestation INTEGER, name TEXT);
        CREATE TABLE pairs (a TEXT, b INTEGER, v TEXT, PRIMARY KEY (b, a)) WITHOUT ROWID;
        CREATE UNIQUE INDEX jobs_name ON jobs(name);";

    /// A database with `SCHEMA` and some rows, and its twin.
    fn pair() -> anyhow::Result<(Connection, Connection)> {
        let a = Connection::open_in_memory()?;
        let b = Connection::open_in_memory()?;
        for c in [&a, &b] {
            c.execute_batch(SCHEMA)?;
            c.execute_batch(
                "INSERT INTO runs VALUES ('r1', 1, 'first'), ('r2', 2, 'second');
                 INSERT INTO jobs VALUES ('j1', 'r1', 'build', 1.5, x'00ff'), ('j2', 'r1', 'test', NULL, NULL), ('j3', 'r2', 'lint', 2.0, NULL);
                 INSERT INTO audit (what) VALUES ('made');
                 INSERT INTO subjects VALUES (1, 'a'), (1, 'b');
                 INSERT INTO pairs VALUES ('x', 1, 'one'), ('y', 2, 'two');",
            )?;
        }
        b.execute_batch("PRAGMA foreign_keys = OFF")?;
        Ok((a, b))
    }

    /// Runs `write` on `a` as the store will (a savepoint, the hook on, its change read, the
    /// transaction taken back), applies the change to `b`, and runs `write` on `a` for real: then the
    /// two must be the same database.
    fn same_after(write: impl Fn(&Connection) -> anyhow::Result<()>) -> anyhow::Result<Change> {
        let (a, b) = pair()?;
        let watch = Watch::install(&a, &["local"])?;
        a.execute_batch("SAVEPOINT kiln_call")?;
        watch.begin();
        let done = write(&a);
        let change = watch.end(&a)?;
        a.execute_batch("ROLLBACK TO kiln_call; RELEASE kiln_call")?;
        let mut known = HashMap::new();
        b.execute_batch("BEGIN")?;
        apply(&b, &Change::decode(&change.encode()?)?, &mut known)?;
        b.execute_batch("COMMIT")?;
        // The write for real, as it went inside the savepoint (its errors and all).
        let _ = done;
        let _ = write(&a);
        assert_eq!(dump(&a)?, dump(&b)?, "the replica differs: {change:#?}");
        Ok(change)
    }

    #[test]
    fn rows_keep_their_rowids_and_order_whatever_their_keys() -> anyhow::Result<()> {
        let change = same_after(|c| {
            let tx = crate::store::tx(c)?;
            for i in 0..20 {
                tx.execute("INSERT INTO runs VALUES (?1, ?2, 'many')", rusqlite::params![format!("m{i:02}"), i])?;
            }
            tx.execute("INSERT INTO jobs VALUES ('j9', 'm03', 'deploy', 3.25, x'01')", [])?;
            tx.commit()?;
            Ok(())
        })?;
        assert_eq!(change.rows.len(), 21);
        Ok(())
    }

    #[test]
    fn cascades_rowless_and_keyless_tables_and_autoincrement_come_out_the_same() -> anyhow::Result<()> {
        same_after(|c| {
            c.execute("DELETE FROM runs WHERE id = 'r1'", [])?; // cascades to j1, j2
            c.execute("UPDATE subjects SET name = 'c' WHERE name = 'b'", [])?;
            c.execute("INSERT INTO subjects VALUES (2, 'd')", [])?;
            c.execute("UPDATE pairs SET a = 'z', v = 'moved' WHERE a = 'x'", [])?; // its key changes
            c.execute("DELETE FROM pairs WHERE a = 'y'", [])?;
            c.execute("INSERT INTO audit (what) VALUES ('one'), ('two')", [])?;
            c.execute("DELETE FROM audit WHERE what = 'one'", [])?;
            c.execute("UPDATE runs SET rowid = 77 WHERE id = 'r2'", [])?; // a row moves its rowid
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn what_a_write_took_back_or_failed_at_is_not_in_its_change() -> anyhow::Result<()> {
        let change = same_after(|c| {
            {
                let tx = crate::store::tx(c)?;
                tx.execute("INSERT INTO runs VALUES ('gone', 9, 'taken back')", [])?;
                // dropped: rolled back
            }
            // A statement that fails half way: j1 takes 'zz', then j2 can't, and j1 goes back.
            assert!(c.execute("UPDATE jobs SET name = 'zz' WHERE id IN ('j1', 'j2')", []).is_err());
            c.execute("INSERT INTO runs VALUES ('kept', 10, 'stays')", [])?;
            Ok(())
        })?;
        let firsts: Vec<(&str, Option<&Val>)> = change.rows.iter().filter_map(|r| if let Row::Kept { table, values, .. } = r { Some((table.as_str(), values.first())) } else { None }).collect();
        let text = |t: &str| Val::Text(t.as_bytes().to_vec());
        assert!(firsts.contains(&("runs", Some(&text("kept")))), "{firsts:?}");
        assert!(!firsts.contains(&("runs", Some(&text("gone")))), "what it took back isn't kept: {firsts:?}");
        Ok(())
    }

    #[test]
    fn a_write_that_failed_after_writing_keeps_what_it_kept() -> anyhow::Result<()> {
        same_after(|c| {
            c.execute("INSERT INTO runs VALUES ('before', 11, 'autocommitted')", [])?;
            anyhow::bail!("then it failed")
        })?;
        Ok(())
    }

    #[test]
    fn values_of_every_kind_and_unique_swaps_come_out_the_same() -> anyhow::Result<()> {
        same_after(|c| {
            c.execute("UPDATE jobs SET name = 'tmp' WHERE id = 'j1'", [])?;
            c.execute("UPDATE jobs SET name = 'build' WHERE id = 'j2'", [])?;
            c.execute("UPDATE jobs SET name = 'test', score = -0.0, data = x'' WHERE id = 'j1'", [])?;
            c.execute("INSERT INTO runs VALUES ('bytes', 12, CAST(x'c328' AS TEXT))", [])?; // not UTF-8
            c.execute("INSERT INTO runs VALUES ('big', 9223372036854775807, NULL)", [])?;
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn a_read_changes_nothing_and_a_node_s_own_tables_are_refused() -> anyhow::Result<()> {
        let (a, _) = pair()?;
        a.execute_batch("CREATE TABLE local (k TEXT PRIMARY KEY, v TEXT)")?;
        let watch = Watch::install(&a, &["local"])?;
        a.execute_batch("SAVEPOINT kiln_call")?;
        watch.begin();
        let n: i64 = a.query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))?;
        assert_eq!(n, 3);
        assert!(watch.end(&a)?.is_empty(), "a read is no change");
        watch.begin();
        a.execute("INSERT INTO local VALUES ('k', 'v')", [])?;
        let refused = watch.end(&a);
        assert!(refused.is_err_and(|e| e.to_string().contains("this node's own tables: local")));
        a.execute_batch("ROLLBACK TO kiln_call; RELEASE kiln_call")?;
        Ok(())
    }
}
