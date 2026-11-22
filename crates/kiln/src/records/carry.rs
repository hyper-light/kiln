//! Goals and plans carried from kiln's SQLite store into Records, row by row, and compared field by
//! field (CP-T1.5, T-T1.5.2; G9 step 4; kiln goal 0d717247). Each table of [`TABLES`] becomes the
//! family `kiln.<table>`; each row a record whose key is its primary key and whose value is every
//! column with SQLite's storage class kept ([`Field`]), so [`compare`] tells a text `1` from an
//! integer 1, keeps a real's every bit, and finds a text that isn't UTF-8 rather than smoothing it
//! over. What the copy carries is what the tables hold: nothing of kiln's model is between them, so
//! a column a later migration adds is carried and compared without a change here.
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

/// Every table of goals and plans in kiln's store (docs/records-inventory.md).
pub const TABLES: &[&str] = &["goal_stacks", "goal_items", "plans", "plan_versions", "plan_runs", "plan_run_steps", "plan_reviews"];

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
}

/// A difference between a table and its family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    pub table: &'static str,
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

/// A page of a table's rows after `after` (a rowid), in rowid order, and the last rowid read.
async fn page(store: &Store, table: &'static str, after: i64) -> anyhow::Result<(Vec<Read>, i64)> {
    store
        .read(move |c| {
            let mut info = c.prepare(&format!("SELECT name, pk FROM pragma_table_info('{table}') ORDER BY cid"))?;
            let cols = info.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            anyhow::ensure!(!cols.is_empty(), "kiln's store has no table {table}");
            let mut pk: Vec<(i64, usize)> = cols.iter().enumerate().filter(|(_, (_, k))| *k > 0).map(|(i, (_, k))| (*k, i)).collect();
            pk.sort_unstable();
            let list = cols.iter().map(|(n, _)| format!("\"{n}\"")).collect::<Vec<_>>().join(", ");
            let mut s = c.prepare(&format!("SELECT rowid, {list} FROM \"{table}\" WHERE rowid > ?1 ORDER BY rowid LIMIT ?2"))?;
            let limit = i64::try_from(PAGE)?;
            let mut rows = s.query(rusqlite::params![after, limit])?;
            let (mut out, mut last) = (Vec::new(), after);
            while let Some(r) = rows.next()? {
                last = r.get::<_, i64>(0)?;
                let mut fields = Fields::new();
                for (i, (name, _)) in cols.iter().enumerate() {
                    fields.insert(name.clone(), Field::of(r.get_ref(i + 1)?));
                }
                // The primary key's fields in order; a table without one is keyed by its rowid.
                let key: Vec<Field> = if pk.is_empty() { vec![Field::Integer(last)] } else { pk.iter().filter_map(|(_, i)| cols.get(*i).and_then(|(n, _)| fields.get(n)).cloned()).collect() };
                let key = serde_json::to_vec(&key)?;
                anyhow::ensure!(key.len() <= KEY_MAX, "a row of {table} has a primary key of {} bytes, over the Records seam's {KEY_MAX}", key.len());
                out.push(Read { key, fields });
            }
            Ok((out, last))
        })
        .await
}

fn refused(r: Refusal) -> anyhow::Error {
    anyhow::anyhow!("records: {r}")
}

/// Puts every row of [`TABLES`] into its family, a page a commit. How many rows it carried.
pub async fn carry(store: &Store, records: &dyn Records) -> anyhow::Result<usize> {
    let mut carried = 0;
    for &table in TABLES {
        let fam = family(table);
        let mut after = i64::MIN;
        loop {
            let (rows, last) = page(store, table, after).await?;
            if rows.is_empty() {
                break;
            }
            carried += rows.len();
            let mut writes = Vec::with_capacity(rows.len());
            for r in rows {
                writes.push(Write::Put { family: fam.clone(), key: r.key, value: serde_json::to_vec(&r.fields)? });
            }
            records.commit(Tx { conds: Vec::new(), writes }).await.map_err(|e| anyhow::anyhow!("carrying {table}: {}", refused(e)))?;
            after = last;
        }
    }
    Ok(carried)
}

/// Reads every row of [`TABLES`] and every record of their families, and lists each difference.
pub async fn compare(store: &Store, records: &dyn Records) -> anyhow::Result<Compared> {
    let mut out = Compared::default();
    for &table in TABLES {
        let fam = family(table);
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
        let mut after = i64::MIN;
        loop {
            let (rows, last) = page(store, table, after).await?;
            if rows.is_empty() {
                break;
            }
            out.rows += rows.len();
            for r in rows {
                let key = String::from_utf8_lossy(&r.key).into_owned();
                let Some(value) = kept.remove(&r.key) else {
                    out.differences.push(Difference { table, key, differs: Differs::NoRecord });
                    continue;
                };
                let have: Fields = match serde_json::from_slice(&value) {
                    Ok(f) => f,
                    Err(e) => {
                        out.differences.push(Difference { table, key, differs: Differs::Unreadable(e.to_string()) });
                        continue;
                    }
                };
                for d in fields_differ(&r.fields, &have) {
                    out.differences.push(Difference { table, key: key.clone(), differs: d });
                }
            }
            after = last;
        }
        for key in kept.into_keys() {
            out.differences.push(Difference { table, key: String::from_utf8_lossy(&key).into_owned(), differs: Differs::NoRow });
        }
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

    /// Two rows in every table of goals and plans, each column given a value of its declared type
    /// (the second row's nullable columns NULL), whatever columns the migrations made.
    async fn fill(store: &Store) -> anyhow::Result<()> {
        store
            .call(|c| {
                for &table in TABLES {
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
                        c.execute(&format!("INSERT INTO \"{table}\" ({names}) VALUES ({marks})"), rusqlite::params_from_iter(values))?;
                    }
                }
                Ok(())
            })
            .await
    }

    /// T-T1.5.2 (CP-T1.5): every goal and plan carried into Records reads back the same, field by
    /// field; a field changed, a row added or one deleted after the copy is found, each by its
    /// table, key and column; carrying again makes the two the same again.
    #[test]
    fn t_t1_5_2_goals_and_plans_carried_into_records_compare_field_by_field() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let store = Store::in_memory()?;
            fill(&store).await?;
            let records = SimRecords::new();
            let carried = carry(&store, &records).await?;
            anyhow::ensure!(carried == 2 * TABLES.len(), "every row carried: {carried}");
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
                    (d.table, what)
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
            let left: Vec<(&str, &Differs)> = again.differences.iter().map(|d| (d.table, &d.differs)).collect();
            anyhow::ensure!(left == vec![("plans", &Differs::NoRow)], "only the deleted row's record is left: {left:?}");
            Ok(())
        })
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
