//! Every table of kiln's SQLite store carried into Records, row by row, and compared field by field
//! (CP-T1.5, T-T1.5.2; G9 step 4; kiln goal 0d717247): goals and plans first, and with them every
//! other record kind the store keeps (docs/records-inventory.md). Each table [`tables`] reads from
//! the store as it stands becomes the family `kiln.<table>`; each row a record whose key is its primary key and whose value is every
//! column with SQLite's storage class kept ([`Field`]), so [`compare`] tells a text `1` from an
//! integer 1, keeps a real's every bit, and finds a text that isn't UTF-8 rather than smoothing it
//! over. What the copy carries is what the tables hold: nothing of kiln's model is between them, so
//! a column or a table a later migration adds is carried and compared without a change here.
//!
//! [`carry`] puts every row (again, when it's run again: a copy is idempotent), a page of rows a
//! commit; [`compare`] reads every row back and lists each difference: a row with no record, a
//! record with no row, a field missing, added or different. The rows are read in pages as the store
//! stands at each page, so a copy taken while kiln writes goals can differ from the tables by the
//! writes made meanwhile; [`compare`] says so row by row, which is what the move's switch-over waits
//! on (its writes stopped, carried, compared empty, then switched).

use std::collections::BTreeMap;

use kiln_seams::records::{Consistency, KEY_MAX, Records, Refusal, SCAN_MAX, Scan, TX_MAX, Tx, Write};
use rusqlite::types::ValueRef;
use serde::{Deserialize, Serialize};

use crate::store::Store;

/// Every table and virtual table of a SQLite database by name, and whether it's WITHOUT ROWID.
const TABLE_LIST: &str = "SELECT name, wr FROM pragma_table_list WHERE schema = 'main' AND type IN ('table', 'virtual') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name";

/// A table of kiln's store, as [`tables`] finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub name: String,
    /// A WITHOUT ROWID table: its rows are paged by its primary key, as it has no rowid.
    without_rowid: bool,
}

/// Every table of kiln's store that keeps rows, by name: its tables and its virtual tables (a
/// full-text index such as `ci_log_index` keeps its text itself). Not SQLite's own (`sqlite_*`), a
/// virtual table's shadow tables (its storage, carried through it), views, or the tables Records
/// itself is kept in ([`crate::records::STORE_TABLES`]). Read from the store as it stands
/// (`pragma_table_list`), so a table a later migration adds is carried without a change here.
pub async fn tables(store: &Store) -> anyhow::Result<Vec<Table>> {
    store
        .read(|c| {
            let mut s = c.prepare(TABLE_LIST)?;
            let all = s.query_map([], |r| Ok(Table { name: r.get(0)?, without_rowid: r.get::<_, i64>(1)? != 0 }))?.collect::<Result<Vec<_>, _>>()?;
            Ok(all.into_iter().filter(|t| !crate::records::STORE_TABLES.contains(&t.name.as_str())).collect())
        })
        .await
}

/// Rows read, and records written, at a time: as many as one commit takes (the Records seam's own
/// bound, `TX_MAX`).
const PAGE: usize = TX_MAX;

/// One field as SQLite keeps it, its storage class with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    Null,
    Integer(i64),
    /// A real's bits (`f64::to_bits`): exact, and a value JSON can always hold.
    Real(u64),
    Text(String),
    /// A text that isn't UTF-8, as hex.
    TextBytes(String),
    /// A blob, as hex.
    Blob(String),
}

impl Field {
    fn of(v: ValueRef<'_>) -> Self {
        match v {
            ValueRef::Null => Field::Null,
            ValueRef::Integer(i) => Field::Integer(i),
            ValueRef::Real(f) => Field::Real(f.to_bits()),
            ValueRef::Text(t) => match std::str::from_utf8(t) {
                Ok(s) => Field::Text(s.to_owned()),
                Err(_) => Field::TextBytes(hex::encode(t)),
            },
            ValueRef::Blob(b) => Field::Blob(hex::encode(b)),
        }
    }
}

/// A row: each column by name.
pub type Fields = BTreeMap<String, Field>;

/// One row as read from a table: its key in Records, and its fields.
struct Read {
    key: Vec<u8>,
    fields: Fields,
}

/// The family a table's rows are kept in.
pub fn family(table: &str) -> String {
    format!("kiln.{table}")
}

/// How a difference between a table and its family shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Differs {
    /// A row with no record.
    NoRecord,
    /// A record with no row.
    NoRow,
    /// A field that differs, or is on one side only (`None`).
    Field { column: String, row: Option<Field>, record: Option<Field> },
    /// A record whose value isn't a row's encoding.
    Unreadable(String),
    /// Between two Records ([`compare_records`]): a record whose value differs, each side's length.
    Value { from: usize, to: usize },
}

/// A difference between a table and its family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    pub table: String,
    /// The row's key, as its primary key's fields in JSON.
    pub key: String,
    pub differs: Differs,
}

/// What [`compare`] found.
#[derive(Debug, Default)]
pub struct Compared {
    /// Rows read from the tables.
    pub rows: usize,
    /// Records read from the families.
    pub records: usize,
    pub differences: Vec<Difference>,
}

/// A field as SQLite gave it, kept to bind again: where a WITHOUT ROWID table's next page starts.
#[derive(Debug, Clone)]
enum Raw {
    Null,
    Integer(i64),
    Real(f64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

impl Raw {
    fn of(v: ValueRef<'_>) -> Self {
        match v {
            ValueRef::Null => Raw::Null,
            ValueRef::Integer(i) => Raw::Integer(i),
            ValueRef::Real(f) => Raw::Real(f),
            ValueRef::Text(t) => Raw::Text(t.to_vec()),
            ValueRef::Blob(b) => Raw::Blob(b.to_vec()),
        }
    }
}

impl rusqlite::ToSql for Raw {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::Borrowed(match self {
            Raw::Null => ValueRef::Null,
            Raw::Integer(i) => ValueRef::Integer(*i),
            Raw::Real(f) => ValueRef::Real(*f),
            Raw::Text(t) => ValueRef::Text(t),
            Raw::Blob(b) => ValueRef::Blob(b),
        }))
    }
}

/// Where a table's next page starts.
#[derive(Debug, Clone)]
enum After {
    /// Its first row.
    Start,
    /// After a rowid.
    Rowid(i64),
    /// After a WITHOUT ROWID table's primary key, its fields in the key's order.
    Key(Vec<Raw>),
}

/// A page of a table's rows after `after`, in rowid order (a WITHOUT ROWID table's: its primary
/// key's), and where the next one starts.
async fn page(store: &Store, table: &Table, after: After) -> anyhow::Result<(Vec<Read>, After)> {
    let Table { name: table, without_rowid } = table.clone();
    store
        .read(move |c| {
            let mut info = c.prepare(&format!("SELECT name, pk FROM pragma_table_info('{table}') ORDER BY cid"))?;
            let cols = info.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            anyhow::ensure!(!cols.is_empty(), "kiln's store has no table {table}");
            let mut pk: Vec<(i64, usize)> = cols.iter().enumerate().filter(|(_, (_, k))| *k > 0).map(|(i, (_, k))| (*k, i)).collect();
            pk.sort_unstable();
            anyhow::ensure!(!without_rowid || !pk.is_empty(), "{table} has no rowid and no primary key");
            let list = cols.iter().map(|(n, _)| format!("\"{n}\"")).collect::<Vec<_>>().join(", ");
            let keys = pk.iter().filter_map(|(_, i)| cols.get(*i).map(|(n, _)| format!("\"{n}\""))).collect::<Vec<_>>().join(", ");
            let limit = i64::try_from(PAGE)?;
            // The fields after which this page starts, bound in order, then the page's size.
            let (from, bound): (String, Vec<Raw>) = match after {
                After::Start => (String::new(), Vec::new()),
                After::Rowid(r) => ("WHERE rowid > ?".to_owned(), vec![Raw::Integer(r)]),
                After::Key(k) => {
                    let marks = vec!["?"; k.len()].join(", ");
                    (format!("WHERE ({keys}) > ({marks})"), k)
                }
            };
            let sql = if without_rowid { format!("SELECT {list} FROM \"{table}\" {from} ORDER BY {keys} LIMIT ?") } else { format!("SELECT rowid, {list} FROM \"{table}\" {from} ORDER BY rowid LIMIT ?") };
            let mut s = c.prepare(&sql)?;
            let mut params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|b| b as &dyn rusqlite::ToSql).collect();
            params.push(&limit);
            let mut rows = s.query(params.as_slice())?;
            // Where a WITHOUT ROWID table's columns start in its rows: it has no rowid before them.
            let skip = usize::from(!without_rowid);
            let (mut out, mut next) = (Vec::new(), None);
            while let Some(r) = rows.next()? {
                let mut fields = Fields::new();
                for (i, (name, _)) in cols.iter().enumerate() {
                    fields.insert(name.clone(), Field::of(r.get_ref(i + skip)?));
                }
                // The primary key's fields in order; a table without one is keyed by its rowid.
                let key: Vec<Field> = if pk.is_empty() { vec![Field::Integer(r.get::<_, i64>(0)?)] } else { pk.iter().filter_map(|(_, i)| cols.get(*i).and_then(|(n, _)| fields.get(n)).cloned()).collect() };
                next = Some(if without_rowid { After::Key(pk.iter().map(|(_, i)| r.get_ref(i + skip).map(Raw::of)).collect::<Result<_, _>>()?) } else { After::Rowid(r.get::<_, i64>(0)?) });
                let key = serde_json::to_vec(&key)?;
                anyhow::ensure!(key.len() <= KEY_MAX, "a row of {table} has a primary key of {} bytes, over the Records seam's {KEY_MAX}", key.len());
                out.push(Read { key, fields });
            }
            Ok((out, next.unwrap_or(After::Start)))
        })
        .await
}

fn refused(r: Refusal) -> anyhow::Error {
    anyhow::anyhow!("records: {r}")
}

/// Puts every row of every table [`tables`] finds into its family, a page a commit. How many rows it carried.
pub async fn carry(store: &Store, records: &dyn Records) -> anyhow::Result<usize> {
    let mut carried = 0;
    for table in tables(store).await? {
        let fam = family(&table.name);
        let mut after = After::Start;
        loop {
            let (rows, next) = page(store, &table, after).await?;
            if rows.is_empty() {
                break;
            }
            carried += rows.len();
            let mut writes = Vec::with_capacity(rows.len());
            for r in rows {
                writes.push(Write::Put { family: fam.clone(), key: r.key, value: serde_json::to_vec(&r.fields)? });
            }
            records.commit(Tx { conds: Vec::new(), writes }).await.map_err(|e| anyhow::anyhow!("carrying {}: {}", table.name, refused(e)))?;
            after = next;
        }
    }
    Ok(carried)
}

/// Reads every row of every table [`tables`] finds and every record of their families, and lists each difference.
pub async fn compare(store: &Store, records: &dyn Records) -> anyhow::Result<Compared> {
    let mut out = Compared::default();
    for table in tables(store).await? {
        let fam = family(&table.name);
        let at = Consistency::Snapshot(records.head().await.map_err(refused)?);
        // Every record of the family, by key.
        let mut kept: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut after = None;
        loop {
            let p = records.scan(&fam, Scan { prefix: Vec::new(), after: after.take(), limit: SCAN_MAX, at }).await.map_err(refused)?;
            after = p.rows.last().map(|r| r.key.clone());
            out.records += p.rows.len();
            kept.extend(p.rows.into_iter().map(|r| (r.key, r.value)));
            if !p.more || after.is_none() {
                break;
            }
        }
        let mut after = After::Start;
        loop {
            let (rows, next) = page(store, &table, after).await?;
            if rows.is_empty() {
                break;
            }
            out.rows += rows.len();
            for r in rows {
                let key = String::from_utf8_lossy(&r.key).into_owned();
                let Some(value) = kept.remove(&r.key) else {
                    out.differences.push(Difference { table: table.name.clone(), key, differs: Differs::NoRecord });
                    continue;
                };
                let have: Fields = match serde_json::from_slice(&value) {
                    Ok(f) => f,
                    Err(e) => {
                        out.differences.push(Difference { table: table.name.clone(), key, differs: Differs::Unreadable(e.to_string()) });
                        continue;
                    }
                };
                for d in fields_differ(&r.fields, &have) {
                    out.differences.push(Difference { table: table.name.clone(), key: key.clone(), differs: d });
                }
            }
            after = next;
        }
        for key in kept.into_keys() {
            out.differences.push(Difference { table: table.name.clone(), key: String::from_utf8_lossy(&key).into_owned(), differs: Differs::NoRow });
        }
    }
    Ok(out)
}

/// Every family of the Records kiln's store keeps ([`crate::records::StoreRecords`]: deploys' and
/// their fenced leases among them), by name, from its own list of them (`records_families`): the
/// Records seam lists no families, so a copy between two stores starts from the one that keeps it.
pub async fn families(store: &Store) -> anyhow::Result<Vec<String>> {
    store
        .read(|c| {
            let mut s = c.prepare("SELECT family FROM records_families ORDER BY family")?;
            let all = s.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            Ok(all)
        })
        .await
}

/// Every record of `family` in `records` as they stand at `at`, by key.
async fn every(records: &dyn Records, family: &str, at: Consistency) -> anyhow::Result<BTreeMap<Vec<u8>, Vec<u8>>> {
    let mut out = BTreeMap::new();
    let mut after = None;
    loop {
        let p = records.scan(family, Scan { prefix: Vec::new(), after: after.take(), limit: SCAN_MAX, at }).await.map_err(refused)?;
        after = p.rows.last().map(|r| r.key.clone());
        out.extend(p.rows.into_iter().map(|r| (r.key, r.value)));
        if !p.more || after.is_none() {
            return Ok(out);
        }
    }
}

/// Every record of each of `families` in `from`, read at one snapshot (its head as the copy
/// starts), put into `to` as it is (its key and value; `to` gives it a commit of its own), a page a
/// commit (CP-T1.5, CP-T1.6: the server's Records, deploys under fenced leases among them, onto
/// the store `records::open` chooses). A lease's fence is in its value, and each conditional write
/// reads the revision it names afresh, so what the copy keeps is what a writer after the
/// switch-over needs; as with [`carry`], writers stop before it, and [`compare_records`] says
/// whether anything moved meanwhile. How many records it put.
pub async fn copy_records(from: &dyn Records, families: &[String], to: &dyn Records) -> anyhow::Result<usize> {
    let at = Consistency::Snapshot(from.head().await.map_err(refused)?);
    let mut copied = 0;
    for fam in families {
        let all: Vec<(Vec<u8>, Vec<u8>)> = every(from, fam, at).await?.into_iter().collect();
        for page in all.chunks(PAGE) {
            let writes = page.iter().map(|(key, value)| Write::Put { family: fam.clone(), key: key.clone(), value: value.clone() }).collect();
            to.commit(Tx { conds: Vec::new(), writes }).await.map_err(|e| anyhow::anyhow!("copying {fam}: {}", refused(e)))?;
            copied += page.len();
        }
    }
    Ok(copied)
}

/// Each record of `families` that differs between `from` and `to`, each read at its head: one in
/// `from` with none in `to` ([`Differs::NoRecord`]), one in `to` alone ([`Differs::NoRow`]), or a
/// value that differs ([`Differs::Value`]). [`Difference::table`] is the family; its key is shown
/// as hex.
pub async fn compare_records(from: &dyn Records, families: &[String], to: &dyn Records) -> anyhow::Result<Vec<Difference>> {
    let (a, b) = (Consistency::Snapshot(from.head().await.map_err(refused)?), Consistency::Snapshot(to.head().await.map_err(refused)?));
    let mut out = Vec::new();
    for fam in families {
        let mut theirs = every(to, fam, b).await?;
        for (key, value) in every(from, fam, a).await? {
            let differs = match theirs.remove(&key) {
                None => Some(Differs::NoRecord),
                Some(v) if v != value => Some(Differs::Value { from: value.len(), to: v.len() }),
                Some(_) => None,
            };
            if let Some(differs) = differs {
                out.push(Difference { table: fam.clone(), key: hex::encode(&key), differs });
            }
        }
        out.extend(theirs.into_keys().map(|key| Difference { table: fam.clone(), key: hex::encode(&key), differs: Differs::NoRow }));
    }
    Ok(out)
}

/// Each column whose field differs between a row and its record, or is on one side only.
fn fields_differ(row: &Fields, record: &Fields) -> Vec<Differs> {
    let mut out = Vec::new();
    for (column, f) in row {
        match record.get(column) {
            Some(g) if g == f => {}
            g => out.push(Differs::Field { column: column.clone(), row: Some(f.clone()), record: g.cloned() }),
        }
    }
    for (column, g) in record {
        if !row.contains_key(column) {
            out.push(Differs::Field { column: column.clone(), row: None, record: Some(g.clone()) });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use kiln_seams::records::SimRecords;

    use super::*;

    /// A store at `path` whose every table [`tables`] finds holds two rows, besides what its
    /// migrations put in, each column given a value of its declared type (the second row's
    /// nullable columns NULL), whatever columns and tables the migrations made; and how many rows
    /// its tables hold. The rows are written on a connection of their own before the store opens,
    /// with the store's checks put aside so each table's rows stand alone: foreign keys off, CHECK
    /// constraints not applied, and the triggers dropped while it writes and made again after, so a
    /// row in one table makes none in another.
    async fn filled(path: &std::path::Path) -> anyhow::Result<(Store, usize)> {
        {
            // Made as the store makes itself (its schema, then its migrations), then its checks off.
            let mut c = rusqlite::Connection::open(path)?;
            c.execute_batch(crate::store::SCHEMA)?;
            crate::store::migrations::apply(&mut c)?;
            c.execute_batch("PRAGMA foreign_keys = OFF; PRAGMA ignore_check_constraints = ON")?;
            let triggers: Vec<(String, String)> = c.prepare("SELECT name, sql FROM sqlite_schema WHERE type = 'trigger' AND sql IS NOT NULL")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
            for (name, _) in &triggers {
                c.execute_batch(&format!("DROP TRIGGER \"{name}\""))?;
            }
            let all: Vec<String> = c.prepare(TABLE_LIST)?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
            for table in all.iter().filter(|t| !crate::records::STORE_TABLES.contains(&t.as_str())) {
                let mut info = c.prepare(&format!("SELECT name, type, \"notnull\", pk FROM pragma_table_info('{table}') ORDER BY cid"))?;
                let cols = info.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? != 0, r.get::<_, i64>(3)? > 0)))?.collect::<Result<Vec<_>, _>>()?;
                drop(info);
                for n in 0..2_i64 {
                    let names = cols.iter().map(|(c, ..)| format!("\"{c}\"")).collect::<Vec<_>>().join(", ");
                    let values: Vec<rusqlite::types::Value> = cols
                        .iter()
                        .map(|(name, ty, notnull, pk)| {
                            let ty = ty.to_ascii_uppercase();
                            if n == 1 && !notnull && !pk {
                                rusqlite::types::Value::Null
                            } else if ty.contains("INT") {
                                rusqlite::types::Value::Integer(1_000_000_007 * (n + 1))
                            } else if ty.contains("REAL") || ty.contains("FLOA") || ty.contains("DOUB") {
                                rusqlite::types::Value::Real(if n == 0 { 0.1 } else { 1.1 })
                            } else if ty.contains("BLOB") {
                                rusqlite::types::Value::Blob(vec![0, 255, u8::try_from(n).unwrap_or(0)])
                            } else {
                                rusqlite::types::Value::Text(format!("{table}.{name} {n}: ünïcode, \"quoted\", 'single'"))
                            }
                        })
                        .collect();
                    let marks = (1..=values.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ");
                    c.execute(&format!("INSERT INTO \"{table}\" ({names}) VALUES ({marks})"), rusqlite::params_from_iter(values)).map_err(|e| anyhow::anyhow!("two rows in {table}: {e}"))?;
                }
            }
            for (_, sql) in &triggers {
                c.execute_batch(sql)?;
            }
        }
        let store = Store::open(path)?;
        // Its index migrations built now, as a store in memory's are: built later, each would add
        // its row to `migrations` while the copy ran.
        store.build_now()?;
        let mut rows = 0;
        for table in tables(&store).await? {
            let name = table.name.clone();
            rows += store.read(move |c| Ok(c.query_row(&format!("SELECT count(*) FROM \"{name}\""), [], |r| r.get::<_, i64>(0))?)).await?;
        }
        Ok((store, usize::try_from(rows)?))
    }

    /// T-T1.5.2 (CP-T1.5): every row of every table of kiln's store carried into Records reads
    /// back the same, field by field; a field changed, a row added or one deleted after the copy is
    /// found, each by its table, key and column; carrying again makes the two the same again.
    #[test]
    fn t_t1_5_2_every_table_carried_into_records_compares_field_by_field() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let (store, rows) = filled(&dir.path().join("kiln.db")).await?;
            let found = tables(&store).await?;
            anyhow::ensure!(rows >= 2 * found.len(), "two rows in each of {} tables: {rows}", found.len());
            let records = SimRecords::new();
            let carried = carry(&store, &records).await?;
            anyhow::ensure!(carried == rows, "every row carried: {carried} of {rows}");
            let same = compare(&store, &records).await?;
            anyhow::ensure!(same.differences.is_empty(), "no difference: {:?}", same.differences);
            anyhow::ensure!(same.rows == carried && same.records == carried, "{} rows, {} records", same.rows, same.records);

            // After the copy: a field changed (an integer column given a text), a row deleted, and a
            // row's record lost.
            store
                .call(|c| {
                    c.execute("UPDATE goal_items SET pos = 'first' WHERE pos = 1000000007", [])?;
                    c.execute("DELETE FROM plans WHERE rowid = (SELECT MIN(rowid) FROM plans)", [])?;
                    Ok(())
                })
                .await?;
            let stacks = family("goal_stacks");
            let first = records.scan(&stacks, Scan { prefix: Vec::new(), after: None, limit: 1, at: Consistency::Linearizable }).await.map_err(refused)?;
            let lost = first.rows.first().map(|r| r.key.clone()).ok_or_else(|| anyhow::anyhow!("a goal stack's record"))?;
            records.commit(Tx { conds: Vec::new(), writes: vec![Write::Delete { family: stacks, key: lost }] }).await.map_err(refused)?;
            let found = compare(&store, &records).await?;
            let mut kinds: Vec<(&str, String)> = found
                .differences
                .iter()
                .map(|d| {
                    let what = match &d.differs {
                        Differs::Field { column, row: Some(Field::Text(_)), record: Some(Field::Integer(_)) } => format!("field {column} integer to text"),
                        other => format!("{other:?}"),
                    };
                    (d.table.as_str(), what)
                })
                .collect();
            kinds.sort();
            anyhow::ensure!(
                kinds == vec![("goal_items", "field pos integer to text".to_owned()), ("goal_stacks", "NoRecord".to_owned()), ("plans", "NoRow".to_owned())],
                "each difference found, by table and column: {kinds:?}"
            );

            // Carried again, the lost record and the changed field are in Records; the deleted
            // row's record stays (a copy puts, it doesn't delete) and is still found.
            carry(&store, &records).await?;
            let again = compare(&store, &records).await?;
            let left: Vec<(&str, &Differs)> = again.differences.iter().map(|d| (d.table.as_str(), &d.differs)).collect();
            anyhow::ensure!(left == vec![("plans", &Differs::NoRow)], "only the deleted row's record is left: {left:?}");
            Ok(())
        })
    }

    /// T-T1.5.2 on each engine `records::open` chooses (CP-T1.5): Turso's, the control, and
    /// Amphora's. Every row of every table of kiln's store carried reads back the same, field by
    /// field, and a field changed after the copy is found by its table and column.
    #[test]
    fn t_t1_5_2_every_table_carries_and_compares_the_same_on_each_engine() -> anyhow::Result<()> {
        for engine in crate::records::Engine::ALL {
            crate::rt::block_on(async move {
                let dir = tempfile::tempdir()?;
                let records = engine.open(&dir.path().join("records.db"))?;
                let (store, rows) = filled(&dir.path().join("kiln.db")).await?;
                let carried = carry(&store, &*records).await?;
                let same = compare(&store, &*records).await?;
                anyhow::ensure!(same.differences.is_empty() && same.records == carried && carried == rows, "{engine:?}: {carried} of {rows} carried, {} read back, {:?}", same.records, same.differences);
                store.call(|c| Ok(c.execute("UPDATE goal_items SET pos = 'first' WHERE pos = 1000000007", [])?)).await?;
                let found = compare(&store, &*records).await?;
                let what: Vec<(&str, Option<&str>)> = found.differences.iter().map(|d| (d.table.as_str(), if let Differs::Field { column, .. } = &d.differs { Some(column.as_str()) } else { None })).collect();
                anyhow::ensure!(what == [("goal_items", Some("pos"))], "{engine:?}: the changed field found: {what:?}");
                Ok(())
            })?;
        }
        Ok(())
    }

    /// CP-T1.5 / CP-T1.6 on each engine `records::open` chooses: the server's Records (a deploy's
    /// release and its fenced lease) copied into a records file of Turso's and of Amphora's read back
    /// the same, record by record, and a value changed after is found by family and key.
    #[test]
    fn cp_t1_5_the_servers_records_copy_and_compare_the_same_on_each_engine() -> anyhow::Result<()> {
        for engine in crate::records::Engine::ALL {
            crate::rt::block_on(async move {
                let dir = tempfile::tempdir()?;
                let to = engine.open(&dir.path().join("records.db"))?;
                let store = Store::in_memory()?;
                let from = crate::records::StoreRecords::open(store.clone())?;
                let put = |f: &str, k: &str, v: &str| Write::Put { family: f.into(), key: k.as_bytes().to_vec(), value: v.as_bytes().to_vec() };
                from.commit(Tx { conds: Vec::new(), writes: vec![put("deploy.release", "r1", "{\"id\":1}"), put("deploy.lease", "web", "fence 7")] }).await?;
                let fams = families(&store).await?;
                let copied = copy_records(&from, &fams, &*to).await?;
                let same = compare_records(&from, &fams, &*to).await?;
                anyhow::ensure!(copied == 2 && same.is_empty(), "{engine:?}: {copied} copied, {same:?}");
                from.commit(Tx { conds: Vec::new(), writes: vec![put("deploy.lease", "web", "fence 8")] }).await?;
                let found: Vec<(String, Differs)> = compare_records(&from, &fams, &*to).await?.into_iter().map(|d| (d.table, d.differs)).collect();
                anyhow::ensure!(found == vec![("deploy.lease".to_owned(), Differs::Value { from: 7, to: 7 })], "{engine:?}: {found:?}");
                Ok(())
            })?;
        }
        Ok(())
    }

    /// A WITHOUT ROWID table is paged by its primary key, a page a commit: past one page, every
    /// row is carried once and read back the same, and a row changed after the copy is found.
    #[test]
    fn t_t1_5_2_a_table_without_rowid_is_carried_page_by_page() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let store = Store::in_memory()?;
            let found = tables(&store).await?;
            anyhow::ensure!(found.iter().any(|t| t.name == "plan_tags" && t.without_rowid), "plan_tags is WITHOUT ROWID: {found:?}");
            let n = PAGE + PAGE / 2;
            store
                .call(move |c| {
                    for i in 0..n {
                        c.execute("INSERT INTO plan_tags (tag, plan_id) VALUES (?1, ?2)", rusqlite::params![format!("t{}", i % 7), format!("p{i}")])?;
                    }
                    Ok(())
                })
                .await?;
            let records = SimRecords::new();
            let carried = carry(&store, &records).await?;
            let same = compare(&store, &records).await?;
            anyhow::ensure!(same.differences.is_empty(), "{:?}", same.differences);
            let tags = records.scan(&family("plan_tags"), Scan { prefix: Vec::new(), after: None, limit: SCAN_MAX, at: Consistency::Linearizable }).await.map_err(refused)?;
            let mut all = tags.rows.len();
            let mut after = tags.rows.last().map(|r| r.key.clone());
            let mut more = tags.more;
            while more {
                let p = records.scan(&family("plan_tags"), Scan { prefix: Vec::new(), after: after.take(), limit: SCAN_MAX, at: Consistency::Linearizable }).await.map_err(refused)?;
                all += p.rows.len();
                after = p.rows.last().map(|r| r.key.clone());
                more = p.more && after.is_some();
            }
            anyhow::ensure!(all == n && carried >= n, "{all} of {n} tags carried ({carried} rows in all)");
            store.call(|c| Ok(c.execute("DELETE FROM plan_tags WHERE plan_id = 'p3'", [])?)).await?;
            let found = compare(&store, &records).await?;
            let what: Vec<(&str, &Differs)> = found.differences.iter().map(|d| (d.table.as_str(), &d.differs)).collect();
            anyhow::ensure!(what == vec![("plan_tags", &Differs::NoRow)], "{what:?}");
            Ok(())
        })
    }

    /// CP-T1.5 / CP-T1.6: the Records the server keeps in its store (deploys' among them) copied
    /// family by family into another store read back the same, record by record; a record changed,
    /// one lost and one added after the copy are each found by family and key; copying again leaves
    /// only the added one.
    #[test]
    fn cp_t1_5_the_servers_records_copied_into_another_store_compare_record_by_record() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let store = Store::in_memory()?;
            let from = crate::records::StoreRecords::open(store.clone())?;
            let put = |f: &str, k: &str, v: &str| Write::Put { family: f.into(), key: k.as_bytes().to_vec(), value: v.as_bytes().to_vec() };
            // A commit's most, and one more after: the copy takes more than one page.
            let writes: Vec<Write> = (0..TX_MAX).map(|i| put("deploy.release", &format!("r{i}"), &format!("{{\"id\":{i}}}"))).collect();
            from.commit(Tx { conds: Vec::new(), writes }).await?;
            from.commit(Tx { conds: Vec::new(), writes: vec![put("deploy.release", "late", "1"), put("deploy.lease", "web", "fence 7"), put("deploy.lease", "gone", "x")] }).await?;
            from.commit(Tx { conds: Vec::new(), writes: vec![Write::Delete { family: "deploy.lease".into(), key: b"gone".to_vec() }] }).await?;
            let fams = families(&store).await?;
            anyhow::ensure!(fams == ["deploy.lease", "deploy.release"], "{fams:?}");
            let to = SimRecords::new();
            let copied = copy_records(&from, &fams, &to).await?;
            anyhow::ensure!(copied == TX_MAX + 2, "every record kept, the deleted one not: {copied}");
            let same = compare_records(&from, &fams, &to).await?;
            anyhow::ensure!(same.is_empty(), "{same:?}");
            from.commit(Tx { conds: Vec::new(), writes: vec![put("deploy.lease", "web", "fence 8"), Write::Delete { family: "deploy.release".into(), key: b"r1".to_vec() }] }).await?;
            to.commit(Tx { conds: Vec::new(), writes: vec![put("deploy.release", "stray", "1")] }).await.map_err(refused)?;
            let mut found: Vec<(String, String, Differs)> = compare_records(&from, &fams, &to).await?.into_iter().map(|d| (d.table, d.key, d.differs)).collect();
            found.sort_by(|x, y| (&x.0, &x.1).cmp(&(&y.0, &y.1)));
            let want = vec![
                ("deploy.lease".to_owned(), hex::encode("web"), Differs::Value { from: 7, to: 7 }),
                ("deploy.release".to_owned(), hex::encode("r1"), Differs::NoRow),
                ("deploy.release".to_owned(), hex::encode("stray"), Differs::NoRow),
            ];
            anyhow::ensure!(found == want, "{found:?}");
            copy_records(&from, &fams, &to).await?;
            let left: Vec<(String, Differs)> = compare_records(&from, &fams, &to).await?.into_iter().map(|d| (d.key, d.differs)).collect();
            anyhow::ensure!(left == vec![(hex::encode("r1"), Differs::NoRow), (hex::encode("stray"), Differs::NoRow)], "a copy puts, it doesn't delete: {left:?}");
            Ok(())
        })
    }

    /// CP-T1.5 (goal 0d717247, every kiln record on Amphora): every table docs/records-inventory.md
    /// lists as made by kiln's store's migrations, and not dropped since, is one [`tables`] finds in
    /// a store those migrations made, so [`carry`] carries it and [`compare`] compares it; every
    /// table [`tables`] finds is in the inventory, and not as tests only, so none is carried that
    /// it doesn't account for; and Records' own tables are in the store but not carried.
    #[test]
    fn cp_t1_5_every_table_of_kilns_store_in_the_inventory_is_carried() -> anyhow::Result<()> {
        let inventory = include_str!("../../../../docs/records-inventory.md");
        let sqlite = inventory.split("## SQLite tables").nth(1).and_then(|s| s.split("\n## ").next()).ok_or_else(|| anyhow::anyhow!("the inventory's SQLite tables"))?;
        // Each row: its table, where it's made, where it's dropped, and whether only tests make it.
        let rows: Vec<Vec<&str>> = sqlite.lines().filter(|l| l.starts_with("| `")).map(|l| l.split('|').map(|c| c.trim().trim_matches('`')).collect()).collect();
        let listed: BTreeMap<&str, (&str, &str, &str)> = rows.iter().filter_map(|r| match r.as_slice() {
            [_, name, made, dropped, tests, ..] => Some((*name, (*made, *dropped, *tests))),
            _ => None,
        }).collect();
        anyhow::ensure!(listed.len() > 100, "the inventory's tables read: {}", listed.len());
        let found: Vec<String> = crate::rt::block_on(async move { Ok::<_, anyhow::Error>(tables(&Store::in_memory()?).await?.into_iter().map(|t| t.name).collect()) })?;
        let mut missing: Vec<&str> = listed.iter().filter(|(_, (made, dropped, tests))| made.contains("crates/kiln/src/store/migrations/") && dropped.is_empty() && *tests != "yes").map(|(name, _)| *name).filter(|name| !crate::records::STORE_TABLES.contains(name) && !found.iter().any(|f| f == name)).collect();
        missing.sort_unstable();
        anyhow::ensure!(missing.is_empty(), "made by the store's migrations but not carried: {missing:?}");
        let unlisted: Vec<&String> = found.iter().filter(|f| !listed.contains_key(f.as_str())).collect();
        anyhow::ensure!(unlisted.is_empty(), "carried but not in the inventory: {unlisted:?}");
        // A table kiln's store keeps is kiln's, whatever file makes it (the store's own SCHEMA too).
        let as_tests: Vec<&String> = found.iter().filter(|f| listed.get(f.as_str()).is_some_and(|(_, _, tests)| *tests == "yes")).collect();
        anyhow::ensure!(as_tests.is_empty(), "kept by kiln's store, but in the inventory as tests only: {as_tests:?}");
        for own in crate::records::STORE_TABLES {
            anyhow::ensure!(listed.contains_key(own) && !found.iter().any(|f| f == own), "{own}: Records' own, in the inventory and not carried");
        }
        Ok(())
    }

    /// Each storage class survives the trip: a real's bits, a text that isn't UTF-8, a blob.
    #[test]
    fn t_t1_5_2_fields_keep_their_storage_class() -> anyhow::Result<()> {
        let fields = vec![Field::Null, Field::Integer(i64::MIN), Field::Real(f64::MIN_POSITIVE.to_bits()), Field::Real((-0.0_f64).to_bits()), Field::Text("é\u{0}".into()), Field::TextBytes("ff00".into()), Field::Blob(String::new())];
        let back: Vec<Field> = serde_json::from_slice(&serde_json::to_vec(&fields)?)?;
        anyhow::ensure!(back == fields, "{back:?}");
        anyhow::ensure!(Field::of(ValueRef::Text(&[0xff, 0x00])) == Field::TextBytes("ff00".into()), "a text that isn't UTF-8 is kept as its bytes");
        anyhow::ensure!(Field::of(ValueRef::Real(-0.0)) != Field::of(ValueRef::Real(0.0)), "a real's sign is kept");
        Ok(())
    }
}
