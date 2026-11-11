//! Physical scratch for a communication transaction, separate from learned receiver depth.
//! SQLite's file-format and WAL-format invariants are recorded in queue-prevention/ROOT.md.

use super::Store;

#[cfg(test)]
#[path = "storage_tests.rs"]
mod wal_tests;

impl Store {
    /// Preflight the storage scratch for an inserted original, an optional superseded status
    /// and its usage counter. Actual transaction failure never transfers custody.
    pub async fn storage_reserve(&self, max_publication_bytes: u64) -> anyhow::Result<u64> {
        let changes = ["original", "pending-status", "usage"];
        self.storage_reserve_tables(max_publication_bytes, vec!["agent_mail".into(), "agent_mail_usage".into()], u64::try_from(changes.len())?).await
    }

    /// Callers give the actual affected tables, total encoded payload and maximum changed rows.
    /// Layout is read from SQLite; the estimate cannot reserve against unrelated disk writers.
    pub async fn storage_reserve_tables(&self, max_payload_bytes: u64, mut tables: Vec<String>, rows: u64) -> anyhow::Result<u64> {
        anyhow::ensure!(rows > 0 && !tables.is_empty(), "storage scratch needs affected rows and tables");
        tables.sort();
        tables.dedup();
        self.read(move |connection| {
            let page_size = u64::try_from(connection.query_row("PRAGMA page_size", [], |r| r.get::<_, i64>(0))?)?;
            let pages = u64::try_from(connection.query_row("PRAGMA page_count", [], |r| r.get::<_, i64>(0))?)?;
            let mut trees = 0u64;
            let mut columns = 0u64;
            for table in &tables {
                let count = u64::try_from(connection.query_row("SELECT count(*) FROM sqlite_schema WHERE rootpage > 0 AND tbl_name=?1", [table], |r| r.get::<_, i64>(0))?)?;
                let width = u64::try_from(connection.query_row("SELECT count(*) FROM pragma_table_info(?1)", [table], |r| r.get::<_, i64>(0))?)?;
                anyhow::ensure!(count > 0 && width > 0, "unknown affected storage table");
                trees = trees.checked_add(count).ok_or_else(|| anyhow::anyhow!("storage tree count overflow"))?;
                columns = columns.max(width);
            }
            let sequence = u64::try_from(connection.query_row("SELECT count(*) FROM sqlite_schema WHERE name='sqlite_sequence' AND rootpage>0", [], |r| r.get::<_, i64>(0))?)?;
            trees = trees.checked_add(sequence).ok_or_else(|| anyhow::anyhow!("storage tree count overflow"))?;
            reserve_rows(page_size, pages, trees, columns, max_payload_bytes, rows)
        }).await
    }
}

#[cfg(test)]
fn reserve(page_size: u64, pages: u64, trees: u64, columns: u64, publication: u64) -> anyhow::Result<u64> {
    reserve_rows(page_size, pages, trees, columns, publication, 1)
}

fn reserve_rows(page_size: u64, pages: u64, trees: u64, columns: u64, publication: u64, rows: u64) -> anyhow::Result<u64> {
    anyhow::ensure!((512..=65536).contains(&page_size) && page_size.is_power_of_two() && trees > 0 && columns > 0 && rows > 0, "unknown communication storage layout");
    let overflow = || anyhow::anyhow!("communication storage reservation overflow");
    let add = |a: u64, b: u64| a.checked_add(b).ok_or_else(overflow);
    let mul = |a: u64, b: u64| a.checked_mul(b).ok_or_else(overflow);
    let ceil = |a: u64, b: u64| a.checked_div(b).zip(a.checked_rem(b)).and_then(|(whole, rest)| whole.checked_add(u64::from(rest != 0))).ok_or_else(overflow);

    // A row retains the envelope and its separately indexed identity columns. Record serial
    // types and row ids use at most nine bytes per varint. Count headers for every changed row
    // at the largest actual table width; identities may occur beside the encoded payload.
    let record = add(mul(publication, 2)?, mul(mul(add(columns, 1)?, 9)?, rows)?)?;
    // Usable pages are at least 480 bytes, of which an overflow next-pointer consumes four.
    // One additional root/leaf per tree covers the local record and a possible new root.
    let payload_pages = add(ceil(record, 480 - 4)?, rows)?;
    // SQLite's balanced trees have minimum fanout two (three except the root). A balance
    // redistributes at most NB=3 old nodes into at most NB+2=5 new nodes. Bound tree depth
    // from the actual page count and possible new leaves, rather than rewriting the entire
    // database for each row. The source derivation and live 29GB counterexample are in
    // queue-prevention/SQLITE-STORAGE.md. The extra root covers SQLite's virtual root page.
    let old_siblings = 3;
    let new_siblings = add(old_siblings, 2)?;
    let leaves = add(pages, mul(mul(rows, trees)?, new_siblings)?)?;
    let possible_pages = add(mul(leaves, 2)?, 1)?;
    let depth = add(u64::from(possible_pages.checked_ilog2().ok_or_else(overflow)?), 2)?;
    let balanced = mul(mul(mul(rows, trees)?, depth)?, add(old_siblings, new_siblings)?)?;
    let overflow_pages = mul(payload_pages, trees)?;
    let changed = add(balanced, overflow_pages)?;
    // Count a freelist trunk and pointer-map update for every changed page, plus the header.
    // Treat all those pages as growth too. This overcounts reuse and disabled auto-vacuum;
    // the floor remains a preflight, not a reservation against concurrent unrelated writers.
    let bookkeeping = add(mul(changed, 2)?, 1)?;
    let new_pages = add(changed, bookkeeping)?;
    let wal_pages = add(new_pages, overflow_pages)?;
    // WAL: 32-byte header, 24-byte frame header plus each full database page. The WAL index
    // needs 32768-byte blocks; even its first block holds 4062 frame-number entries.
    let wal = add(32, mul(wal_pages, add(page_size, 24)?)?)?;
    let index = mul(ceil(wal_pages, 4062)?, 32768)?;
    add(add(mul(new_pages, page_size)?, wal)?, index)
}

#[cfg(test)]
mod tests {
    use super::{reserve, reserve_rows};

    #[test]
    fn storage_scratch_accounts_for_database_pages_and_wal() -> anyhow::Result<()> {
        let small = reserve(4096, 100, 5, 15, 1024)?;
        assert!(small > 100 * 4096);
        assert!(reserve(4096, 200, 5, 15, 1024)? > small);
        assert!(reserve(4096, 100, 5, 15, 2048)? > small);
        assert!(reserve(4096, 100, 6, 15, 1024)? > small);
        assert!(reserve(4096, 100, 5, 15, u64::MAX).is_err());
        assert!(reserve(0, 100, 5, 15, 1024).is_err());
        Ok(())
    }

    #[test]
    fn a_large_database_reserves_changed_tree_paths_instead_of_every_old_page_per_row() -> anyhow::Result<()> {
        // Read-only live server layout, recorded in sqlite-reserve-before.json. The former
        // whole-database multiplier asked for 442753176600 bytes and refused this ordinary send.
        let scratch = reserve_rows(4096, 7_158_475, 5, 15, 512 * 1024, 3)?;
        assert!(scratch < 268_829_614_080);
        assert!(scratch > 512 * 1024);
        assert!(reserve_rows(4096, 7_158_475, 5, 15, u64::MAX, 3).is_err());
        Ok(())
    }
}
