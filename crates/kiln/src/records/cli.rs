//! `kiln records`: every table of kiln's store (its goals and plans among them) carried into Records
//! and compared with them, from the command line (CP-T1.5, T-T1.5.2; kiln goal 0d717247). `kiln
//! records carry` puts every row of every table [`carry::tables`] finds into its family, then compares; `kiln records compare` only compares. Each
//! difference is printed by table, key and column, and the command fails when there's one, so the
//! move's switch-over can wait on it.
//!
//! Into the records the server keeps in its store (kiln.db), which is written: with the server
//! stopped. With `--into`, a records file opened by [`super::open`] (the one place that chooses the
//! engine under Records), kiln.db only read, so it may run beside a server; and the Records the
//! server keeps there (deploys' and their fenced leases among them) are copied into it and compared
//! too, family by family ([`carry::copy_records`]).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use kiln_seams::records::Records;

use super::carry::{self, Compared, Differs, Field};
use crate::config::Config;
use crate::store::Store;

#[derive(clap::Args)]
pub struct RecordsArgs {
    #[command(subcommand)]
    cmd: RecordsCmd,
}

#[derive(clap::Subcommand)]
enum RecordsCmd {
    /// Put every row of every table of kiln's store into Records (each row whole, so running it again
    /// copies again), then compare the two.
    Carry(Where),
    /// Compare every table of kiln's store with its records: each row with no record, record with
    /// no row, and field that differs, by table, key and column. Fails when any differs.
    Compare(Where),
}

#[derive(clap::Args)]
struct Where {
    /// kiln's store (default: this server's kiln.db).
    #[arg(long)]
    store: Option<PathBuf>,
    /// A records file to carry into and compare with, kept by the engine `records::open` chooses.
    /// Without it: the records the server keeps in kiln.db, which is written, so stop the server
    /// first.
    #[arg(long)]
    into: Option<PathBuf>,
}

pub async fn run(args: RecordsArgs, cfg: &Config) -> Result<()> {
    let (w, carrying) = match args.cmd {
        RecordsCmd::Carry(w) => (w, true),
        RecordsCmd::Compare(w) => (w, false),
    };
    let path = w.store.unwrap_or_else(|| cfg.data("kiln.db"));
    anyhow::ensure!(path.is_file(), "no kiln store at {}", path.display());
    let (store, records): (Store, Arc<dyn Records>) = match &w.into {
        Some(into) => (Store::open_replica(&path)?, super::open(into)?),
        None => {
            let store = Store::open(&path)?;
            let records = Arc::new(super::StoreRecords::open(store.clone())?);
            (store, records)
        }
    };
    let (carried, found) = carry_and_compare(&store, &*records, carrying).await?;
    if let Some(n) = carried {
        println!("carried {n} rows");
    }
    for line in report(&found) {
        println!("{line}");
    }
    // Into another store, the Records the server keeps (deploys' and their leases among them) go
    // too, family by family (CP-T1.5, CP-T1.6).
    let moved = match &w.into {
        Some(_) => {
            let server = super::StoreRecords::open(store.clone())?;
            let families = carry::families(&store).await?;
            if carrying {
                println!("copied {} of the server's records", carry::copy_records(&server, &families, &*records).await?);
            }
            let differ = carry::compare_records(&server, &families, &*records).await?;
            for line in records_report(&differ) {
                println!("{line}");
            }
            differ.len()
        }
        None => 0,
    };
    anyhow::ensure!(found.differences.is_empty() && moved == 0, "the tables and their records differ in {} places, the server's records and their copies in {moved}", found.differences.len());
    Ok(())
}

/// What [`carry::compare_records`] found, a line a record (family, key in hex), then how many.
pub fn records_report(differ: &[carry::Difference]) -> Vec<String> {
    let mut out: Vec<String> = differ
        .iter()
        .map(|d| match &d.differs {
            Differs::NoRecord => format!("{} {}: the server's record, not copied", d.table, d.key),
            Differs::NoRow => format!("{} {}: a copy the server doesn't keep", d.table, d.key),
            Differs::Value { from, to } => format!("{} {}: a value that differs ({from} bytes, {to} copied)", d.table, d.key),
            other => format!("{} {}: {other:?}", d.table, d.key),
        })
        .collect();
    out.push(format!("the server's records: {} differences", differ.len()));
    out
}

/// Carries, when asked, then compares: how many rows were carried, and what the comparison found.
pub async fn carry_and_compare(store: &Store, records: &dyn Records, carrying: bool) -> Result<(Option<usize>, Compared)> {
    let carried = if carrying { Some(carry::carry(store, records).await?) } else { None };
    Ok((carried, carry::compare(store, records).await?))
}

/// What a comparison found, a line a difference (table, key, column, each side's field), then how
/// many rows and records were read and how many places differ.
pub fn report(c: &Compared) -> Vec<String> {
    let shown = |f: &Option<Field>| match f {
        Some(f) => serde_json::to_string(f).unwrap_or_else(|_| format!("{f:?}")),
        None => "missing".to_owned(),
    };
    let mut out: Vec<String> = c
        .differences
        .iter()
        .map(|d| match &d.differs {
            Differs::NoRecord => format!("{} {}: a row with no record", d.table, d.key),
            Differs::NoRow => format!("{} {}: a record with no row", d.table, d.key),
            Differs::Field { column, row, record } => format!("{} {} {column}: row {}, record {}", d.table, d.key, shown(row), shown(record)),
            Differs::Unreadable(why) => format!("{} {}: a record that isn't a row ({why})", d.table, d.key),
            Differs::Value { from, to } => format!("{} {}: a value that differs ({from} bytes, {to} in the record)", d.table, d.key),
        })
        .collect();
    out.push(format!("{} rows, {} records: {} differences", c.rows, c.records, c.differences.len()));
    out
}

#[cfg(test)]
mod tests {
    use kiln_seams::records::{SimRecords, Tx, Write};

    use super::*;
    use crate::records::carry::Difference;

    /// T-T1.5.2: each difference is reported by its table, key and column, with each side's field.
    #[test]
    fn t_t1_5_2_a_comparison_reports_each_difference_by_table_key_and_column() {
        let c = Compared {
            rows: 3,
            records: 3,
            differences: vec![
                Difference { table: "goal_items".into(), key: "[7]".into(), differs: Differs::Field { column: "pos".into(), row: Some(Field::Text("first".into())), record: Some(Field::Integer(1)) } },
                Difference { table: "goal_items".into(), key: "[8]".into(), differs: Differs::Field { column: "added".into(), row: None, record: Some(Field::Null) } },
                Difference { table: "goal_stacks".into(), key: "[\"a\"]".into(), differs: Differs::NoRecord },
                Difference { table: "plans".into(), key: "[\"p\"]".into(), differs: Differs::NoRow },
            ],
        };
        assert_eq!(
            report(&c),
            vec![
                r#"goal_items [7] pos: row {"text":"first"}, record {"integer":1}"#.to_owned(),
                r#"goal_items [8] added: row missing, record "null""#.to_owned(),
                r#"goal_stacks ["a"]: a row with no record"#.to_owned(),
                r#"plans ["p"]: a record with no row"#.to_owned(),
                "3 rows, 3 records: 4 differences".to_owned(),
            ]
        );
    }

    /// T-T1.5.2: what the command runs: carried, the two compare the same; a record with no row,
    /// put after, is found by its table and key, and carrying again doesn't take it away.
    #[test]
    fn t_t1_5_2_carry_then_compare_finds_a_record_with_no_row() -> Result<()> {
        crate::rt::block_on(async {
            let store = Store::in_memory()?;
            let records = SimRecords::new();
            // A new store's rows are what its migrations put in (each migration's own row among them).
            let (carried, same) = carry_and_compare(&store, &records, true).await?;
            let n = carried.ok_or_else(|| anyhow::anyhow!("carried"))?;
            anyhow::ensure!(n > 0 && same.rows == n && same.records == n && same.differences.is_empty(), "a new store carries and compares the same: {carried:?}, {:?}", same.differences);
            records.commit(Tx { conds: Vec::new(), writes: vec![Write::Put { family: carry::family("plans"), key: br#"["stray"]"#.to_vec(), value: b"{}".to_vec() }] }).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            let (_, found) = carry_and_compare(&store, &records, true).await?;
            let lines = report(&found);
            anyhow::ensure!(lines == [r#"plans ["stray"]: a record with no row"#.to_owned(), format!("{n} rows, {} records: 1 differences", n + 1)], "{lines:?}");
            Ok(())
        })
    }
}
