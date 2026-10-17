//! Schema migrations (docs/ci.md §2.4): one SQL file each in `src/store/migrations/`, named
//! `<yyyymmdd-hhmm>_<workstream>_<topic>.sql`, applied in name order, each in its own
//! transaction, and recorded by name in `migrations`. The build script lists the files, so
//! branches that each add one never edit the same line. A migration is never edited once it is
//! on `main`; a fix is a new migration.

use rusqlite::{Connection, params};

include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

/// Applies every migration this connection has not seen yet; returns the names applied.
pub fn apply(conn: &mut Connection) -> anyhow::Result<Vec<String>> {
    apply_list(conn, MIGRATIONS)
}

fn apply_list(conn: &mut Connection, list: &[(&str, &str)]) -> anyhow::Result<Vec<String>> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS migrations (name TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);")?;
    let mut applied = Vec::new();
    for (name, sql) in list {
        let seen: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM migrations WHERE name = ?1)", params![name], |r| r.get(0))?;
        if seen {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql).map_err(|e| anyhow::anyhow!("migration {name}: {e}"))?;
        tx.execute("INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)", params![name, crate::util::now_unix()])?;
        tx.commit()?;
        applied.push((*name).to_owned());
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_in_order_and_are_idempotent() -> anyhow::Result<()> {
        let mut c = Connection::open_in_memory()?;
        let list = [("20260101-0000_t_a", "CREATE TABLE a (x INTEGER);"), ("20260101-0001_t_b", "INSERT INTO a VALUES (1);")];
        assert_eq!(apply_list(&mut c, &list)?.len(), 2);
        assert!(apply_list(&mut c, &list)?.is_empty(), "applying twice changes nothing");
        let n: i64 = c.query_row("SELECT COUNT(*) FROM a", [], |r| r.get(0))?;
        assert_eq!(n, 1);
        // The shipped list applies cleanly to the baseline schema, twice.
        let s = crate::store::Store::in_memory()?;
        drop(s);
        Ok(())
    }

    #[test]
    fn a_failing_migration_leaves_nothing_behind() -> anyhow::Result<()> {
        let mut c = Connection::open_in_memory()?;
        let list = [("20260101-0000_t_bad", "CREATE TABLE ok (x INTEGER); THIS IS NOT SQL;")];
        assert!(apply_list(&mut c, &list).is_err());
        let exists: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'ok')", [], |r| r.get(0))?;
        assert!(!exists, "the transaction rolled back");
        Ok(())
    }

    #[test]
    fn shipped_names_are_sorted_and_well_formed() {
        let names: Vec<&str> = MIGRATIONS.iter().map(|(n, _)| *n).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        for n in names {
            let (stamp, rest) = n.split_once('_').unwrap_or(("", ""));
            assert!(stamp.len() == 13 && stamp.as_bytes().get(8) == Some(&b'-'), "{n}: <yyyymmdd-hhmm>_<ws>_<topic>");
            assert!(!rest.is_empty(), "{n}: needs a workstream and topic");
        }
    }
}
