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
//!
//! **Index migrations** (the DB query audit, 2026-11-06). A migration of indexes alone (each
//! `CREATE INDEX IF NOT EXISTS`, none unique, no epoch, none named by a later migration, nor by a
//! query that doesn't wait for it: [`later_of`]) isn't applied here: the store's builder makes its
//! indexes (`crate::store::indexes`), after kiln serves, an index a transaction, and records it by
//! name with its last. Until then the queries its indexes serve answer the same, slower. Such a
//! migration is recorded after the ones that follow it, which can't tell: none names its indexes.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::bail;
use rusqlite::{Connection, OptionalExtension as _, params};

use super::indexes::{self, Later};

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

/// Applies every migration this connection has not seen yet but the index migrations, which it leaves
/// to the store's builder ([`later_of`]); returns the names applied, and those it leaves.
pub fn apply(conn: &mut Connection) -> anyhow::Result<(Vec<String>, Vec<Later>)> {
    apply_list(conn, MIGRATIONS)
}

/// The index migrations of `list` that `applied` lacks, each with its indexes ([`indexes::index_of`] of
/// every statement): made of indexes alone, without an epoch, none of whose indexes a later migration
/// names ([`indexes::mentions`]: one that drops, makes again or names an index must find it there, so
/// that migration applies in its turn) or a query names ([`indexes::NAMED`]). Which migrations they are is the list's alone, whatever the database holds.
pub(super) fn later_of(list: &[(&str, &str)], applied: &HashSet<String>) -> Vec<Later> {
    let mut out = Vec::new();
    for (at, (name, sql)) in list.iter().enumerate() {
        if applied.contains(*name) || epoch_of(sql).is_some() {
            continue;
        }
        let made: Option<Vec<indexes::Index>> = indexes::statements(sql).iter().map(|s| indexes::index_of(s)).collect();
        let Some(made) = made.filter(|m| !m.is_empty()) else { continue };
        let named = made.iter().any(|i| indexes::NAMED.contains(&i.name.as_str()));
        let after = list.get(at + 1..).unwrap_or_default();
        let named_later = made.iter().any(|i| after.iter().any(|(_, sql)| indexes::mentions(sql, &i.name)));
        if named || named_later {
            continue;
        }
        out.push(Later { migration: (*name).to_owned(), indexes: made });
    }
    out
}

pub(super) fn apply_list(conn: &mut Connection, list: &[(&str, &str)]) -> anyhow::Result<(Vec<String>, Vec<Later>)> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS migrations (name TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS kiln_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;
    let seen: HashSet<String> = conn.prepare("SELECT name FROM migrations")?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    let left = later_of(list, &seen);
    let mut applied = Vec::new();
    for (name, sql) in list {
        if seen.contains(*name) || left.iter().any(|l| l.migration == *name) {
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
    Ok((applied, left))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_in_order_and_are_idempotent() -> anyhow::Result<()> {
        let mut c = Connection::open_in_memory()?;
        let list = [("20260101-0000_t_a", "CREATE TABLE a (x INTEGER);"), ("20260101-0001_t_b", "INSERT INTO a VALUES (1);")];
        assert_eq!(apply_list(&mut c, &list)?.0.len(), 2);
        assert!(apply_list(&mut c, &list)?.0.is_empty(), "applying twice changes nothing");
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
    /// W27-R08: custody's tables (the Transfers plan's steps 2 and 3): every record and delegation
    /// once by its digest, what each follows, the things each is about by every digest, breaks,
    /// refusals, and hand-overs that went unsigned, one per session and epoch; no epoch raised, so
    /// older builds still open the database.
    #[test]
    fn w27_r08_custody_tables() -> anyhow::Result<()> {
        const MINE: &str = "20261030-0100_w27_custody";
        let mut c = Connection::open_in_memory()?;
        c.execute_batch(crate::store::SCHEMA)?;
        let (before, after): (Vec<(&str, &str)>, Vec<(&str, &str)>) = MIGRATIONS.iter().copied().partition(|(n, _)| *n < MINE);
        apply_list(&mut c, &before)?;
        assert!(after.first().is_some_and(|(n, _)| *n == MINE), "{MINE} is shipped");
        apply_list(&mut c, &after)?;
        assert!(apply_list(&mut c, MIGRATIONS)?.0.is_empty(), "applying again changes nothing");
        let columns = |table: &str| -> anyhow::Result<Vec<String>> { Ok(c.prepare(&format!("SELECT name FROM pragma_table_info('{table}') ORDER BY cid"))?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?) };
        assert_eq!(columns("custody")?, ["digest", "payload_type", "kind", "custodian", "counterpart", "stream", "seq", "under", "envelope", "came_from", "taken_at"]);
        assert_eq!(columns("custody_follows")?, ["record", "follows"]);
        assert_eq!(columns("custody_things")?, ["record", "name", "algorithm", "hex"]);
        // `relay`: the relays' (the Transfers plan's step 13, migration w27_relays).
        assert_eq!(columns("custody_breaks")?, ["id", "record", "kind", "detail", "around", "refused", "at", "relay"]);
        assert_eq!(columns("transfer_refusals")?, ["id", "digest", "act", "signer", "why", "envelope", "came_from", "at", "epoch", "superseded_by"]);
        let insert = "INSERT INTO custody (digest, payload_type, kind, custodian, envelope, taken_at) VALUES ('sha256:aa', 'application/vnd.kiln.custody+json', 'received', 'kiln://hyper-light/server', '{}', 1)";
        c.execute(insert, [])?;
        assert!(c.execute(insert, []).is_err(), "a record is kept once by its digest");
        c.execute("INSERT INTO custody_follows (record, follows) VALUES ('sha256:aa', 'sha256:bb')", [])?;
        assert!(c.execute("INSERT INTO custody_follows (record, follows) VALUES ('sha256:aa', 'sha256:bb')", []).is_err(), "a record follows another once");
        assert!(c.execute("INSERT INTO transfer_refusals (digest, act, at) VALUES ('sha256:cc', 'goals.done', 1)", []).is_err(), "a refusal says why");
        assert_eq!(columns("transfers_unsigned")?, ["id", "session", "epoch", "gave", "took", "why", "at"]);
        let unsigned = "INSERT INTO transfers_unsigned (session, epoch, why, at) VALUES ('s1', 2, 'a runner from before', 1)";
        c.execute(unsigned, [])?;
        assert!(c.execute(unsigned, []).is_err(), "one per session and epoch");
        assert_eq!(read_min_reader(&c)?, 0, "no epoch raised: older builds still open it");
        Ok(())
    }

    #[test]
    fn w27_r01_identities_and_revocations_tables() -> anyhow::Result<()> {
        const MINE: &str = "20261026-1800_w27_identities";
        let mut c = Connection::open_in_memory()?;
        c.execute_batch(crate::store::SCHEMA)?;
        let (before, after): (Vec<(&str, &str)>, Vec<(&str, &str)>) = MIGRATIONS.iter().copied().partition(|(n, _)| *n < MINE);
        apply_list(&mut c, &before)?;
        assert!(after.first().is_some_and(|(n, _)| *n == MINE), "{MINE} is shipped");
        apply_list(&mut c, &after)?;
        assert!(apply_list(&mut c, MIGRATIONS)?.0.is_empty(), "applying again changes nothing");
        let columns = |table: &str| -> anyhow::Result<Vec<String>> { Ok(c.prepare(&format!("SELECT name FROM pragma_table_info('{table}') ORDER BY cid"))?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?) };
        // `session`, `epoch`: the agent session and epoch an agent's identity names (migration dbq_custody).
        assert_eq!(columns("identities")?, ["serial", "kind", "subject", "key_sha256", "not_before", "not_after", "issued_at", "cert_pem", "session", "epoch"]);
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

    /// W27-R08's relays (the Transfers plan's step 13): a break names the relay it was found in,
    /// `''` for a break kiln found elsewhere (every break recorded before); kiln finds the relays'
    /// records by their kind and when it took them. No epoch raised, so older builds still open
    /// the database (they never name the column).
    #[test]
    fn w27_r08_relays_column() -> anyhow::Result<()> {
        const MINE: &str = "20261101-0200_w27_relays";
        let mut c = Connection::open_in_memory()?;
        c.execute_batch(crate::store::SCHEMA)?;
        let (before, after): (Vec<(&str, &str)>, Vec<(&str, &str)>) = MIGRATIONS.iter().copied().partition(|(n, _)| *n < MINE);
        apply_list(&mut c, &before)?;
        c.execute("INSERT INTO custody_breaks (record, kind, detail, at) VALUES ('sha256:aa', 'changed', 'before relays', 1)", [])?;
        assert!(after.first().is_some_and(|(n, _)| *n == MINE), "{MINE} is shipped");
        apply_list(&mut c, &after)?;
        let relay: String = c.query_row("SELECT relay FROM custody_breaks WHERE detail = 'before relays'", [], |r| r.get(0))?;
        assert_eq!(relay, "", "a break from before names no relay");
        c.execute("INSERT INTO custody_breaks (record, kind, detail, at, relay) VALUES ('sha256:bb', 'dropped', 'a gap', 2, 'kiln://hyper-light/person/ada/machine/adacachy/instance/01a14794')", [])?;
        let index: i64 = c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'custody_kind_taken'", [], |r| r.get(0))?;
        assert_eq!(index, 1);
        assert!(epoch_of(MIGRATIONS.iter().find(|(n, _)| *n == MINE).map(|(_, sql)| *sql).unwrap_or_default()).is_none(), "no epoch raised");
        Ok(())
    }

    /// The DB query audit (2026-11-06): migration dbq_custody finds in what was kept before it what kiln keeps with
    /// each certificate and record since: an agent identity's session and epoch, none for another identity's; the agent
    /// session each record leaves its things with (whom a hand-on or a take gives them to, a receipt's signer); and the
    /// session each of kiln's takes is taken for, read in its trail entry, or in its envelope for one kept before the
    /// trail was (its payload's every length, padded or not), none for a take for a person or one it can't read. The
    /// index by the text between a counterpart's `/agent/` and `/epoch/` (w27_custody_by_session's) stays, for a build
    /// from before this one rolled back to, and each record's things stay found by their record.
    #[test]
    fn dbq_custody_finds_what_was_kept_before() -> anyhow::Result<()> {
        use base64::Engine as _;
        const MINE: &str = "20261106-2102_dbq_custody";
        let has_index = |c: &Connection, name: &str| -> anyhow::Result<bool> { Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1)", [name], |r| r.get(0))?) };
        let mut c = Connection::open_in_memory()?;
        c.execute_batch(crate::store::SCHEMA)?;
        let (before, after): (Vec<(&str, &str)>, Vec<(&str, &str)>) = MIGRATIONS.iter().copied().partition(|(n, _)| *n < MINE);
        // An index migration from before, as a store's builder would have built it by then.
        let (_, built_later) = apply_list(&mut c, &before)?;
        for index in built_later.iter().flat_map(|l| &l.indexes) {
            c.execute_batch(&index.sql)?;
        }
        let agent = |s: &str, e: u64| format!("kiln://acme/person/ada/agent/{s}/epoch/{e}");
        let held = |s: &str, e: u64| format!("{}/machine/m/instance/i", agent(s, e));
        let person = "kiln://acme/person/ada".to_owned();
        for (serial, kind, subject) in [("01", "agent", held("s1", 2)), ("02", "agent", agent("s2", 0)), ("03", "person", person.clone()), ("04", "agent", "kiln://acme/person/ada/agent/s3".to_owned())] {
            c.execute("INSERT INTO identities (serial, kind, subject, key_sha256, not_before, not_after, issued_at) VALUES (?1, ?2, ?3, 'k', 1, 2, 1)", params![serial, kind, subject])?;
        }
        let record = "INSERT INTO custody (digest, payload_type, kind, custodian, counterpart, envelope, taken_at) VALUES (?1, 'application/vnd.kiln.custody+json', ?2, ?3, ?4, ?5, ?6)";
        // kiln's takes: their statements, in the trail or only in their envelopes (DSSE's payload, base64 padded).
        let statement = |taken_for: &str, why: &str| serde_json::json!({ "v": 1, "subject": [], "follows": [], "body": { "kind": "taken", "by": "kiln://acme/server", "to": agent("s9", 1), "taken_for": taken_for, "why": why } }).to_string();
        let envelope = |payload: &str| serde_json::json!({ "dsseEnvelope": { "payload": base64::engine::general_purpose::STANDARD.encode(payload), "payloadType": "application/vnd.in-toto+json", "signatures": [] } }).to_string();
        let trail = [("t1", statement(&held("s1", 1), "its machine is away")), ("t5", statement(&person, "taken over"))];
        let only_enveloped: Vec<(&str, String)> = [("t2", agent("s2", 0)), ("t3", agent("s4", 3)), ("t4", held("s5", 0))].into_iter().enumerate().map(|(i, (d, g))| (d, statement(&g, &"w".repeat(i)))).collect();
        let lengths: std::collections::BTreeSet<usize> = only_enveloped.iter().map(|(_, s)| s.len() % 3).collect();
        assert_eq!(lengths.len(), 3, "a payload of each length modulo 3: {only_enveloped:?}");
        for (i, (digest, s)) in trail.iter().enumerate() {
            c.execute(record, params![digest, "taken", "kiln://acme/server", agent("s9", 1), envelope(s), 10])?;
            c.execute("INSERT INTO trail (idx, prev, statement, kind, subjects, taken_at, hash, payload_type, payload) VALUES (?1, '', ?2, 'custody', '[]', 10, '', 'application/vnd.kiln.custody+json', ?3)", params![i64::try_from(i)?, digest, s.as_bytes()])?;
        }
        for (digest, s) in &only_enveloped {
            c.execute(record, params![digest, "taken", "kiln://acme/server", agent("s9", 1), envelope(s), 11])?;
        }
        c.execute(record, params!["t6", "taken", "kiln://acme/server", agent("s9", 1), "{}", 12])?;
        c.execute(record, params!["h1", "handed-on", "kiln://acme/server", agent("s6", 0), "{}", 13])?;
        c.execute(record, params!["r1", "received", held("s1", 2), "kiln://acme/server", "{}", 14])?;
        c.execute(record, params!["p1", "handed-on", "kiln://acme/server", person, "{}", 15])?;
        apply_list(&mut c, &after)?;
        assert!(has_index(&c, "custody_counterpart_session")? && has_index(&c, "custody_things_record")?, "w27_custody_by_session's indexes, kept");
        let named: Vec<(String, Option<String>, Option<i64>)> = c.prepare("SELECT serial, session, epoch FROM identities ORDER BY serial")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
        let some = |s: &str| Some(s.to_owned());
        assert_eq!(named, [("01".to_owned(), some("s1"), Some(2)), ("02".to_owned(), some("s2"), Some(0)), ("03".to_owned(), None, None), ("04".to_owned(), None, None)]);
        let takers: Vec<(String, String, String, String)> = c.prepare("SELECT record, session, kind, taker FROM custody_takers ORDER BY record")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?;
        let to_s9 = |d: &str| (d.to_owned(), "s9".to_owned(), "taken".to_owned(), agent("s9", 1));
        assert_eq!(takers, [("h1".to_owned(), "s6".to_owned(), "handed-on".to_owned(), agent("s6", 0)), ("r1".to_owned(), "s1".to_owned(), "received".to_owned(), held("s1", 2)), to_s9("t1"), to_s9("t2"), to_s9("t3"), to_s9("t4"), to_s9("t5"), to_s9("t6")]);
        let taken_for: Vec<(String, String, i64)> = c.prepare("SELECT record, session, taken_at FROM custody_taken_for ORDER BY record")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
        assert_eq!(taken_for, [("t1".to_owned(), "s1".to_owned(), 10), ("t2".to_owned(), "s2".to_owned(), 11), ("t3".to_owned(), "s4".to_owned(), 11), ("t4".to_owned(), "s5".to_owned(), 11)]);
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
