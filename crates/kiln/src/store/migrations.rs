//! Schema migrations (docs/ci.md §2.4): one SQL file each in `src/store/migrations/`, named
//! `<yyyymmdd-hhmm>_<workstream>_<topic>.sql`, applied in name order, each in its own
//! transaction, and recorded by name in `migrations`. The build script lists the files, so
//! branches that each add one never edit the same line. A migration is never edited once it is
//! on `main`; a fix is a new migration.
//!
//! **Epochs** (docs/resilience.md, G2). An older kiln can run on a database a newer one migrated
//! as long as nothing it does breaks on the change: a new table, a nullable column it never
//! names. A migration older builds can't live with (a NOT NULL column without a default, a
//! dropped or renamed column, a table an older build inserts into by position) says so on its
//! first line, `-- kiln:epoch N`, and this build's [`SCHEMA_EPOCH`] is raised to N with it.
//! Applying it raises the database's `min_reader_epoch`; a build whose epoch is below that
//! refuses to open the database, saying why, rather than failing somewhere later (2026-10-24:
//! a rollback crash-looped on a table a newer kiln had widened). The guard of kiln's own deploy
//! reads it too, and doesn't roll back to a build that couldn't read the data.

use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::bail;
use rusqlite::{Connection, OptionalExtension as _, params};

/// The newest schema epoch this build reads and writes: at least every shipped migration's.
pub const SCHEMA_EPOCH: u32 = 1;

/// The database's `min_reader_epoch`, as last read or raised here (0: none said).
static MIN_READER: AtomicU32 = AtomicU32::new(0);

/// The oldest epoch a build must be to read this process's database, as last seen.
pub fn min_reader() -> u32 {
    MIN_READER.load(Ordering::Relaxed)
}

/// The epoch a migration says it needs: `-- kiln:epoch N` on its first line.
fn epoch_of(sql: &str) -> Option<u32> {
    sql.lines().next()?.trim().strip_prefix("-- kiln:epoch ")?.trim().parse().ok()
}

fn read_min_reader(conn: &Connection) -> anyhow::Result<u32> {
    let has: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'kiln_meta')", [], |r| r.get(0))?;
    if !has {
        return Ok(0);
    }
    let v: Option<String> = conn.query_row("SELECT value FROM kiln_meta WHERE key = 'min_reader_epoch'", [], |r| r.get(0)).optional()?;
    Ok(v.and_then(|v| v.parse().ok()).unwrap_or(0))
}

/// Whether this build may open the database: refused, with why, when a newer kiln migrated it
/// past what this build reads. Also says when the database holds migrations this build doesn't
/// know (a newer kiln ran here) that don't stop it.
pub fn check(conn: &Connection) -> anyhow::Result<()> {
    check_against(conn, SCHEMA_EPOCH, MIGRATIONS)
}

fn check_against(conn: &Connection, epoch: u32, list: &[(&str, &str)]) -> anyhow::Result<()> {
    let need = read_min_reader(conn)?;
    MIN_READER.store(need, Ordering::Relaxed);
    if need > epoch {
        bail!(
            "kiln.db needs a kiln of schema epoch {need} or newer, and this is {epoch}: a newer kiln migrated it past what this one reads. Run the newer kiln again (roll forward); this one would fail on its data (docs/resilience.md, G2)"
        );
    }
    let has: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'migrations')", [], |r| r.get(0))?;
    if has {
        let names: Vec<String> = conn.prepare("SELECT name FROM migrations")?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
        let unknown: Vec<&String> = names.iter().filter(|n| !list.iter().any(|(m, _)| m == n)).collect();
        if !unknown.is_empty() {
            tracing::warn!(migrations = ?unknown, epoch, need, "kiln.db has migrations this build doesn't know (a newer kiln ran here); none needs a newer epoch, so it opens");
        }
    }
    Ok(())
}

include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

/// Applies every migration this connection has not seen yet; returns the names applied.
pub fn apply(conn: &mut Connection) -> anyhow::Result<Vec<String>> {
    apply_list(conn, MIGRATIONS)
}

fn apply_list(conn: &mut Connection, list: &[(&str, &str)]) -> anyhow::Result<Vec<String>> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS migrations (name TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS kiln_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;
    let mut applied = Vec::new();
    for (name, sql) in list {
        let seen: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM migrations WHERE name = ?1)", params![name], |r| r.get(0))?;
        if seen {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql).map_err(|e| anyhow::anyhow!("migration {name}: {e}"))?;
        tx.execute("INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)", params![name, crate::util::now_unix()])?;
        if let Some(e) = epoch_of(sql) {
            let need = read_min_reader(&tx)?.max(e);
            tx.execute("INSERT INTO kiln_meta (key, value) VALUES ('min_reader_epoch', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![need.to_string()])?;
            MIN_READER.store(need, Ordering::Relaxed);
        }
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

    /// A database a newer kiln migrated past what this build reads is refused, saying why; one
    /// that only gained what this build can live with opens (docs/resilience.md, G2).
    #[test]
    fn a_build_refuses_data_past_its_epoch() -> anyhow::Result<()> {
        let mut c = Connection::open_in_memory()?;
        let older = [("20260101-0000_t_a", "CREATE TABLE a (x INTEGER);")];
        let additive = [older[0], ("20260101-0001_t_b", "CREATE TABLE b (y INTEGER);")];
        let breaking = [additive[0], additive[1], ("20260101-0002_t_c", "-- kiln:epoch 2\nALTER TABLE a ADD COLUMN z INTEGER NOT NULL DEFAULT 0;")];
        apply_list(&mut c, &additive)?;
        check_against(&c, 1, &older)?;
        apply_list(&mut c, &breaking)?;
        assert_eq!(read_min_reader(&c)?, 2);
        let refused = check_against(&c, 1, &older).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(refused.contains("needs a kiln of schema epoch 2 or newer, and this is 1"), "{refused}");
        check_against(&c, 2, &breaking)?;
        // An epoch is never lowered.
        apply_list(&mut c, &[("20260101-0003_t_d", "-- kiln:epoch 1\nCREATE TABLE d (w INTEGER);")])?;
        assert_eq!(read_min_reader(&c)?, 2);
        Ok(())
    }

    /// W26-R03: diffs and change requests made before change identities each get one; their
    /// Change-Ids become aliases (the newest diff's, for one used twice) and their numbers are
    /// recorded; the epoch stays this build's.
    #[test]
    fn w26_r03_identities_are_backfilled() -> anyhow::Result<()> {
        const MINE: &str = "20261026-1000_w26_change_identity";
        let mut c = Connection::open_in_memory()?;
        c.execute_batch(crate::store::SCHEMA)?;
        let (before, after): (Vec<(&str, &str)>, Vec<(&str, &str)>) = MIGRATIONS.iter().copied().partition(|(n, _)| *n < MINE);
        apply_list(&mut c, &before)?;
        let row = "INSERT INTO diffs (id, repo_id, number, kind, change_id, title, author, author_kind, state, trunk, created_at, updated_at) VALUES (?1, 'r', ?2, ?3, ?4, 't', 'user:ada', 'human', ?5, 'main', 1, 1)";
        for (id, n, kind, cid, state) in [("d1", 1, "diff", Some("I1111111111111111111111111111111111111111"), "landed"), ("d2", 2, "diff", Some("I2222222222222222222222222222222222222222"), "needs-review"), ("c3", 3, "change_request", None, "needs-review"), ("d4", 4, "diff", Some("I1111111111111111111111111111111111111111"), "draft")] {
            c.execute(row, params![id, n, kind, cid, state])?;
        }
        apply_list(&mut c, &after)?;
        let ids: Vec<(String, String)> = c.prepare("SELECT id, kiln_change FROM diffs ORDER BY number")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
        assert_eq!(ids.len(), 4);
        assert!(ids.iter().all(|(_, k)| kiln_ci::identity::is_kiln_change(k)), "{ids:?}");
        let distinct: std::collections::BTreeSet<&String> = ids.iter().map(|(_, k)| k).collect();
        assert_eq!(distinct.len(), 4, "each its own");
        let change_of = |id: &str| ids.iter().find(|(d, _)| d == id).map(|(_, k)| k.clone()).unwrap_or_default();
        let alias: String = c.query_row("SELECT change FROM change_aliases WHERE kind = 'change-id' AND alias = 'I1111111111111111111111111111111111111111'", [], |r| r.get(0))?;
        assert_eq!(alias, change_of("d4"), "a Change-Id used twice points to the newest diff");
        let numbers: Vec<String> = c.prepare("SELECT number FROM change_numbers WHERE host = 'kiln' ORDER BY number")?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
        assert_eq!(numbers, ["CR3", "D1", "D2", "D4"]);
        let changes: i64 = c.query_row("SELECT COUNT(*) FROM changes", [], |r| r.get(0))?;
        assert_eq!(changes, 4);
        assert_eq!(read_min_reader(&c)?, 0, "no epoch raised: older builds still open it");
        Ok(())
    }

    /// W27-R01: the identities and revocations tables keep each certificate issued (serial, kind,
    /// subject, key, validity, issued at) and each revocation (serial or key, at, reason, by); a
    /// revocation naming neither is refused; applying again changes nothing; no epoch is raised.
    #[test]
    fn w27_r01_identities_and_revocations_tables() -> anyhow::Result<()> {
        const MINE: &str = "20261026-1800_w27_identities";
        let mut c = Connection::open_in_memory()?;
        c.execute_batch(crate::store::SCHEMA)?;
        let (before, after): (Vec<(&str, &str)>, Vec<(&str, &str)>) = MIGRATIONS.iter().copied().partition(|(n, _)| *n < MINE);
        apply_list(&mut c, &before)?;
        assert!(after.first().is_some_and(|(n, _)| *n == MINE), "{MINE} is shipped");
        apply_list(&mut c, &after)?;
        assert!(apply_list(&mut c, MIGRATIONS)?.is_empty(), "applying again changes nothing");
        let columns = |table: &str| -> anyhow::Result<Vec<String>> { Ok(c.prepare(&format!("SELECT name FROM pragma_table_info('{table}') ORDER BY cid"))?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?) };
        assert_eq!(columns("identities")?, ["serial", "kind", "subject", "key_sha256", "not_before", "not_after", "issued_at", "cert_pem"]);
        assert_eq!(columns("revocations")?, ["id", "serial", "key_sha256", "subject", "at", "reason", "by"]);
        c.execute("INSERT INTO identities (serial, kind, subject, key_sha256, not_before, not_after, issued_at) VALUES ('0a', 'machine', 'fedora-2', 'k1', 1, 2, 1)", [])?;
        assert!(c.execute("INSERT INTO identities (serial, kind, subject, key_sha256, not_before, not_after, issued_at) VALUES ('0a', 'machine', 'fedora-2', 'k2', 1, 2, 1)", []).is_err(), "a serial is issued once");
        c.execute("INSERT INTO revocations (serial, subject, at, reason, by) VALUES ('0a', 'fedora-2', 1790000000, 'runner forgotten', 'ada')", [])?;
        c.execute("INSERT INTO revocations (key_sha256, subject, at, reason, by) VALUES ('k1', 'fedora-2', 1790000001, 'key lost', 'sam')", [])?;
        assert!(c.execute("INSERT INTO revocations (subject, at, reason, by) VALUES ('fedora-2', 1, 'x', 'ada')", []).is_err(), "a revocation names a serial or a key");
        assert!(c.execute("INSERT INTO revocations (serial, subject, at, by) VALUES ('0a', 'fedora-2', 1, 'ada')", []).is_err(), "a revocation says why");
        let (at, reason, by): (i64, String, String) = c.query_row("SELECT at, reason, by FROM revocations WHERE serial = '0a'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        assert_eq!((at, reason.as_str(), by.as_str()), (1_790_000_000, "runner forgotten", "ada"));
        assert_eq!(read_min_reader(&c)?, 0, "no epoch raised: older builds still open it");
        Ok(())
    }

    #[test]
    fn shipped_epochs_are_this_builds() {
        for (name, sql) in MIGRATIONS {
            assert!(epoch_of(sql).is_none_or(|e| e <= SCHEMA_EPOCH), "{name} needs epoch {:?}: raise SCHEMA_EPOCH with it", epoch_of(sql));
        }
        assert_eq!(epoch_of("-- kiln:epoch 3\nALTER TABLE x DROP COLUMN y;"), Some(3));
        assert_eq!(epoch_of("CREATE TABLE x (y);\n-- kiln:epoch 3"), None, "only the first line says");
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
