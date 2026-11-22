//! U05: Amphora's implementation of its public SQL session API. The database owns the
//! transaction and durable acknowledgement; the session cannot commit it independently.

use crate::db::{Db, Held};
use amphora_api::Value;
pub use amphora_api::sql::*;
use amphora_engine::{Numeric, Row as NativeRow, Value as Native};
use amphora_engine_parser::{
    ast::fmt::{BlankContext, ToTokens, TokenStream},
    ast::{Cmd, Stmt},
    parser::Parser,
    token::TokenType,
};

fn failed(e: impl std::fmt::Display) -> Error {
    Error::Database(format!("{e:#}"))
}

fn native(value: Value) -> Result<Native> {
    Ok(match value {
        Value::Null => Native::Null,
        Value::Integer(n) => Native::from_i64(n),
        Value::Real(n) => Native::from_f64(n),
        Value::Text(text) => Native::from_text(text),
        Value::Blob(bytes) => Native::from_slice(&bytes).map_err(failed)?,
    })
}
fn owned(value: &Native) -> Value {
    match value {
        Native::Null => Value::Null,
        Native::Numeric(Numeric::Integer(n)) => Value::Integer(*n),
        Native::Numeric(Numeric::Float(n)) => Value::Real(f64::from(*n)),
        Native::Text(value) => Value::Text(value.as_str().to_owned()),
        Native::Blob(value) => Value::Blob(value.to_vec()),
    }
}

/// Reserve product metadata names at the public SQL boundary. Using the parsed token
/// stream distinguishes single-quoted names from string values, including inside triggers.
/// Private Db SQL deliberately does not pass through this check.
pub(crate) fn validate_reserved_sql(sql: &str) -> std::result::Result<(), amphora_api::Refusal> {
    amphora_api::bound("SQL bytes", sql.len(), amphora_api::SQL_MAX)?;
    let mut parser = Parser::new(sql.as_bytes());
    let mut count = 0usize;
    while let Some(command) = parser
        .next_cmd()
        .map_err(|e| amphora_api::invalid("SQL", &e.to_string()))?
    {
        count = count
            .checked_add(1)
            .ok_or_else(|| amphora_api::invalid("SQL", "statement count overflow"))?;
        amphora_api::bound("SQL statements", count, amphora_api::TX_MAX)?;
        validate_reserved_command(&command)?;
    }
    Ok(())
}

fn validate_reserved_command(command: &Cmd) -> std::result::Result<(), amphora_api::Refusal> {
    struct Identifiers {
        tokens: usize,
    }
    impl TokenStream for Identifiers {
        type Error = amphora_api::Refusal;
        fn append(
            &mut self,
            kind: TokenType,
            value: Option<&str>,
        ) -> std::result::Result<(), Self::Error> {
            self.tokens = self
                .tokens
                .checked_add(1)
                .ok_or_else(|| amphora_api::invalid("SQL", "token count overflow"))?;
            // Formatting can add punctuation, but output is never retained. This fixed
            // budget bounds traversal independently of the statement's SQL byte bound.
            amphora_api::bound("SQL validation tokens", self.tokens, amphora_api::SQL_MAX)?;
            if kind == TokenType::TK_ID
                && let Some(name) = value
            {
                let name = match name.as_bytes().first() {
                    Some(b'\'' | b'"' | b'`' | b'[') => name.get(1..).unwrap_or(""),
                    _ => name,
                };
                const RESERVED: &str = "__amphora_";
                if name
                    .get(..RESERVED.len())
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(RESERVED))
                {
                    return Err(amphora_api::invalid(
                        "SQL identifier",
                        "__amphora_* names are reserved for product metadata",
                    ));
                }
            }
            Ok(())
        }
    }
    command.to_tokens(&mut Identifiers { tokens: 0 }, &BlankContext)
}

/// Each SQL text's parses by `validate`, counted in tests only (the unify audit's finding 4: a
/// prepared statement is parsed once, not again on every execute).
#[cfg(test)]
static PARSED: std::sync::Mutex<Option<std::collections::HashMap<String, usize>>> =
    std::sync::Mutex::new(None);

/// How many times `validate` has parsed `sql` in this test process.
#[cfg(test)]
fn parses(sql: &str) -> usize {
    PARSED
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|parsed| parsed.get(sql).copied())
        .unwrap_or(0)
}

fn validate(sql: &str, batch: bool) -> Result<()> {
    #[cfg(test)]
    {
        *PARSED
            .lock()
            .unwrap()
            .get_or_insert_with(Default::default)
            .entry(sql.to_owned())
            .or_default() += 1;
    }
    amphora_api::bound("SQL bytes", sql.len(), amphora_api::SQL_MAX).map_err(failed)?;
    let mut parser = Parser::new(sql.as_bytes());
    let mut count = 0usize;
    while let Some(command) = parser.next_cmd().map_err(failed)? {
        validate_reserved_command(&command)?;
        count = count
            .checked_add(1)
            .ok_or_else(|| failed("SQL statement count overflow"))?;
        amphora_api::bound(
            "SQL statements",
            count,
            if batch { amphora_api::TX_MAX } else { 1 },
        )
        .map_err(failed)?;
        if let Cmd::Stmt(statement) = command {
            let prohibited = matches!(
                statement,
                Stmt::Begin { .. }
                    | Stmt::Commit { .. }
                    | Stmt::Rollback {
                        savepoint_name: None,
                        ..
                    }
                    | Stmt::Attach { .. }
                    | Stmt::Detach { .. }
                    | Stmt::Vacuum { .. }
                    | Stmt::Pragma { .. }
            );
            if prohibited {
                return Err(failed(
                    "transaction and connection control belongs to Amphora; nested savepoints are supported",
                ));
            }
        }
    }
    if count == 0 && !batch {
        return Err(failed("SQL statement is empty"));
    }
    Ok(())
}

/// `sql`'s statement, taken from Db's statement cache or prepared, validated as one statement
/// unless its cache entry carries the mark of an earlier validation. Validation reads the text
/// alone, so a mark can't go stale while its entry stands, and the entry takes its mark with it
/// when it's evicted, replaced or dropped for a schema change or a restore. A miss or an unmarked
/// hit is validated before it's prepared or stepped, and only that validation's success marks it
/// (docs/design/store.md, decision 36).
fn held(db: &Db, sql: &str) -> Result<Held> {
    let mut held = match db.cached(sql).map_err(failed)? {
        Some(held) if held.validated() => return Ok(held),
        Some(held) => {
            if let Err(refused) = validate(sql, false) {
                // Db's private SQL cached it; a refusal here leaves that entry as it was.
                db.give_back(held).map_err(failed)?;
                return Err(refused);
            }
            held
        }
        None => {
            validate(sql, false)?;
            db.prepare_held(sql).map_err(failed)?
        }
    };
    held.mark();
    Ok(held)
}

impl Session for Db {
    fn records_get(
        &self,
        namespace: &str,
        family: &str,
        key: &[u8],
    ) -> Result<Option<amphora_api::Row>> {
        crate::database::session::records_get(self, namespace, family, key)
    }
    fn records_commit(&self, namespace: &str, tx: amphora_api::Tx) -> Result<amphora_api::Rev> {
        crate::database::session::records_commit(self, namespace, tx)
    }
    fn execute_batch(&self, sql: &str) -> Result<()> {
        validate(sql, true)?;
        self.exec(sql).map_err(failed)
    }
    // A text call, a remote client's Query among them, parses only what its cache entry's mark
    // doesn't vouch for. A statement that fails as it steps isn't kept.
    fn query(&self, sql: &str, params: Vec<Value>, first: bool) -> Result<Vec<Vec<Value>>> {
        let mut held = held(self, sql)?;
        let rows = rows(params, first, |params, each| {
            self.run_held(&mut held, params, each)
        })?;
        self.give_back(held).map_err(failed)?;
        Ok(rows)
    }
    // Validation reads the text alone, so the statement validated and held here runs again and
    // again without a parse or a lookup by its text: parsing on every execute cost a sixth to a
    // third of kiln's hot statements (docs/design/store.md, decision 34). Prepared again, as
    // kiln does per request, its text isn't parsed again while its entry keeps the mark.
    fn prepare<'s>(&'s self, sql: &str) -> Result<(Box<dyn Prepared + 's>, usize)> {
        let held = held(self, sql)?;
        let columns = held.columns();
        Ok((
            Box::new(Kept {
                db: self,
                held: Some(held),
            }),
            columns,
        ))
    }
    fn column_count(&self, sql: &str) -> Result<usize> {
        let held = held(self, sql)?;
        let columns = held.columns();
        self.give_back(held).map_err(failed)?;
        Ok(columns)
    }
    fn changes(&self) -> i64 {
        Db::changes(self)
    }
    fn last_insert_rowid(&self) -> i64 {
        Db::last_insert_rowid(self)
    }
}

/// A statement the session prepared, held until its `Statement` is dropped.
struct Kept<'s> {
    db: &'s Db,
    held: Option<Held>,
}
impl Prepared for Kept<'_> {
    fn query(&mut self, params: Vec<Value>, first: bool) -> Result<Vec<Vec<Value>>> {
        let db = self.db;
        let held = self
            .held
            .as_mut()
            .ok_or_else(|| failed("a prepared statement ran after it was given back"))?;
        rows(params, first, |params, each| {
            db.run_held(held, params, each)
        })
    }
}
impl Drop for Kept<'_> {
    fn drop(&mut self) {
        if let Some(held) = self.held.take() {
            // The cache only spares preparing the statement again. One it can't take back (its
            // reset failed, or its lock is poisoned, which refuses the next caller) is
            // finalized here instead.
            let _ = self.db.give_back(held);
        }
    }
}

/// A validated statement's rows, each value owned, within the transport's result bounds: `run`
/// steps the statement with its parameters, handing each row on.
fn rows(
    params: Vec<Value>,
    first: bool,
    run: impl FnOnce(
        Vec<Native>,
        &mut dyn FnMut(&NativeRow) -> anyhow::Result<bool>,
    ) -> anyhow::Result<()>,
) -> Result<Vec<Vec<Value>>> {
    let params = params.into_iter().map(native).collect::<Result<Vec<_>>>()?;
    let mut rows = Vec::new();
    let mut bytes = 0usize;
    run(params, &mut |row| {
        // Match the transport decoder's owned-allocation budget before acknowledging
        // a write with RETURNING: a locally accepted result must fit remotely too.
        bytes = bytes
            .checked_add(std::mem::size_of::<Vec<Value>>())
            .ok_or_else(|| anyhow::anyhow!("SQL result size overflow"))?;
        let mut values = Vec::new();
        for value in row.get_values() {
            let payload = match value {
                Native::Text(s) => s.as_str().len(),
                Native::Blob(b) => b.len(),
                _ => 0,
            };
            bytes = bytes
                .checked_add(std::mem::size_of::<Value>())
                .and_then(|n| n.checked_add(payload))
                .ok_or_else(|| anyhow::anyhow!("SQL result size overflow"))?;
            amphora_api::bound("SQL result bytes", bytes, amphora_api::RESULT_MAX)?;
            values.push(owned(value));
        }
        amphora_api::bound("SQL result bytes", bytes, amphora_api::RESULT_MAX)?;
        let count = rows
            .len()
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("SQL result row count overflow"))?;
        amphora_api::bound("SQL result rows", count, amphora_api::RESULT_MAX / 32)?;
        rows.push(values);
        Ok(!first)
    })
    .map_err(failed)?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use amphora_api::{Cond, Consistency, Records, Refusal, Tx, Write};
    use std::time::{Duration, Instant};

    fn until() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    fn policy(expected: Option<u64>, value: &[u8]) -> Tx {
        Tx {
            conds: vec![match expected {
                Some(rev) => Cond::At {
                    family: "policy".into(),
                    key: b"active".to_vec(),
                    rev,
                },
                None => Cond::Absent {
                    family: "policy".into(),
                    key: b"active".to_vec(),
                },
            }],
            writes: vec![Write::Put {
                family: "policy".into(),
                key: b"active".to_vec(),
                value: value.to_vec(),
            }],
        }
    }

    // U05/U06: public SQL cannot alter the metadata that enforces Records and commit fences.
    #[test]
    fn public_sql_reserves_metadata_names_but_allows_literal_and_comment_text() {
        use super::validate_reserved_sql;
        for sql in [
            "UPDATE __amphora_replica SET applied = 0",
            "DROP TABLE main.\"__AMPHORA_restore\"",
            "DELETE FROM `__AmPhOrA_versions`",
            "SELECT * FROM [__amphora_versions]",
            "DROP TABLE '__amphora_versions'",
            "ALTER TABLE ordinary RENAME TO '__amphora_versions'",
            "CREATE TABLE ordinary('__amphora_hidden' INTEGER)",
            "CREATE VIEW ordinary AS SELECT * FROM __amphora_versions",
            "CREATE TRIGGER hidden AFTER INSERT ON ordinary BEGIN DELETE FROM '__amphora_versions'; END",
            "CREATE TRIGGER hidden AFTER INSERT ON ordinary BEGIN UPDATE [__amphora_replica] SET applied = 0; END",
            "SELECT 1; DROP TABLE __amphora_versions",
        ] {
            assert!(
                matches!(validate_reserved_sql(sql), Err(Refusal::Invalid { what, .. }) if what == "SQL identifier"),
                "{sql}"
            );
        }
        for sql in [
            "SELECT '__amphora_versions', 'DROP TABLE __amphora_restore'",
            "SELECT 1 /* __amphora_replica */ -- __amphora_versions\n",
            "CREATE TABLE ordinary(value TEXT DEFAULT '__amphora_versions')",
            "CREATE TABLE user__amphora_versions(value TEXT)",
            "CREATE TRIGGER ordinary_trigger AFTER INSERT ON ordinary BEGIN SELECT '__amphora_versions'; END",
            "SELECT ':__amphora_versions'",
        ] {
            validate_reserved_sql(sql).unwrap();
        }

        let db = crate::database::Database::in_memory(until()).unwrap();
        db.call(until(), |c| {
            c.execute_batch("CREATE TABLE ordinary(value TEXT)")?;
            c.execute("INSERT INTO ordinary VALUES (?1)", ["__amphora_versions"])?;
            assert!(matches!(c.execute("DELETE FROM __amphora_versions", []), Err(super::Error::Refusal(Refusal::Invalid { .. }))));
            assert!(c.prepare("SELECT * FROM [__AMPHORA_versions]").is_err());
            assert!(c.execute_batch("CREATE TRIGGER hidden AFTER INSERT ON ordinary BEGIN DELETE FROM '__amphora_versions'; END").is_err());
            let literal: String = c.query_row("SELECT value FROM ordinary", [], |r| r.get(0))?;
            assert_eq!(literal, "__amphora_versions");
            c.records_commit("tenant", policy(None, b"private path remains available"))?;
            Ok(())
        }).unwrap();
        assert!(
            db.execute(
                vec![amphora_api::Statement {
                    sql: "DROP TABLE '__amphora_versions'".into(),
                    params: vec![]
                }],
                until()
            )
            .is_err()
        );
        assert_eq!(
            db.namespace("tenant")
                .unwrap()
                .get("policy", b"active", Consistency::Linearizable, until())
                .unwrap()
                .unwrap()
                .value,
            b"private path remains available"
        );
    }

    // U05/U06, CP-T1.5: a policy CAS and its SQL reservation have one durable outcome.
    #[test]
    fn records_policy_and_sql_reservations_commit_and_roll_back_together() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atomic.db");
        let committed;
        {
            let db = crate::database::Database::open(&path, until()).unwrap();
            let first = db
                .call(until(), |c| {
                    c.execute_batch("CREATE TABLE reservations(id INTEGER PRIMARY KEY)")?;
                    let rev = c.records_commit("tenant", policy(None, b"initial"))?;
                    assert_eq!(
                        c.records_get("tenant", "policy", b"active")?.unwrap().rev,
                        rev
                    );
                    Ok(rev)
                })
                .unwrap();
            let records = db.namespace("tenant").unwrap();
            assert_eq!(records.wait("policy", 0, until()).unwrap(), first);

            committed = db
                .call(until(), move |c| {
                    let row = c.records_get("tenant", "policy", b"active")?.unwrap();
                    c.execute("INSERT INTO reservations VALUES (1)", [])?;
                    let next = c.records_commit("tenant", policy(Some(row.rev), b"reserved"))?;
                    assert_eq!(row.rev, first);
                    Ok(next)
                })
                .unwrap();
            assert_eq!(records.wait("policy", first, until()).unwrap(), committed);

            let stale: std::result::Result<(), Refusal> = db.call(until(), move |c| {
                c.execute("INSERT INTO reservations VALUES (2)", [])?;
                c.records_commit("tenant", policy(Some(first), b"stale"))?;
                Ok(())
            });
            assert!(
                matches!(stale, Err(Refusal::Conflict { at: Some(at), .. }) if at == committed)
            );

            let failed: std::result::Result<(), Refusal> = db.call(until(), move |c| {
                c.execute("INSERT INTO reservations VALUES (3)", [])?;
                c.records_commit("tenant", policy(Some(committed), b"discarded"))?;
                anyhow::bail!("policy decision refused")
            });
            assert!(failed.is_err());
            // A nested savepoint discards both APIs and must not publish its provisional revision.
            db.call(until(), move |c| {
                let nested = c.unchecked_transaction()?;
                nested.execute("INSERT INTO reservations VALUES (4)", [])?;
                nested.records_commit("tenant", policy(Some(committed), b"nested"))?;
                nested.rollback()?;
                Ok(())
            })
            .unwrap();
            assert_eq!(records.head(until()).unwrap(), committed);
            assert!(matches!(
                records.wait("policy", committed, Instant::now()),
                Err(Refusal::Timeout)
            ));
            db.call(until(), |c| {
                let count: i64 =
                    c.query_row("SELECT count(*) FROM reservations", [], |r| r.get(0))?;
                assert_eq!(count, 1);
                assert_eq!(
                    c.records_get("tenant", "policy", b"active")?.unwrap().value,
                    b"reserved"
                );
                Ok(())
            })
            .unwrap();
        }
        let db = crate::database::Database::open(&path, until()).unwrap();
        let records = db.namespace("tenant").unwrap();
        assert_eq!(records.wait("policy", 0, until()).unwrap(), committed);
        assert_eq!(
            records
                .get("policy", b"active", Consistency::Linearizable, until())
                .unwrap()
                .unwrap()
                .value,
            b"reserved"
        );
        db.call(until(), |c| {
            let ids: i64 = c.query_row("SELECT sum(id) FROM reservations", [], |r| r.get(0))?;
            assert_eq!(ids, 1);
            Ok(())
        })
        .unwrap();
    }

    // Kiln plan 01a18d12142c7789a40e959ccdf52bd1, step 2. Do: run one prepare_cached
    // `SELECT * FROM held`, add a column, run it again and prepare it again; then prepare one
    // INSERT twice and run the two in turn. Expect: the held statement and the new one both read
    // the added column, and the INSERT's two statements each write their rows (AC-T1.1.1;
    // docs/design/store.md, decision 34).
    #[test]
    fn a_held_statement_follows_the_schema_and_is_never_handed_out_twice() {
        use amphora_api::Value;
        let db = crate::database::Database::in_memory(until()).unwrap();
        let (columns, rows) = db
            .call(until(), |c| {
                c.execute_batch("CREATE TABLE held(a INTEGER); INSERT INTO held VALUES (1)")?;
                let mut all = c.prepare_cached("SELECT * FROM held")?;
                let before = all.query_row([], |r| Ok(r.column_count()))?;
                c.execute_batch("ALTER TABLE held ADD COLUMN c TEXT")?;
                let after = all.query_row([], |r| Ok(r.column_count()))?;
                let again = c
                    .prepare_cached("SELECT * FROM held")?
                    .query_row([], |r| Ok(r.column_count()))?;
                let mut one = c.prepare_cached("INSERT INTO held(a) VALUES (?1)")?;
                let mut two = c.prepare_cached("INSERT INTO held(a) VALUES (?1)")?;
                for n in 2..12i64 {
                    let statement = if n % 2 == 0 { &mut one } else { &mut two };
                    assert_eq!(statement.execute(vec![Value::Integer(n)])?, 1);
                }
                let rows: i64 = c.query_row("SELECT count(*) FROM held", [], |r| r.get(0))?;
                Ok(([before, after, again], rows))
            })
            .unwrap();
        assert_eq!(
            columns,
            [1, 2, 2],
            "one column, then the added one through the held statement and a new one"
        );
        assert_eq!(
            rows, 11,
            "the first row and ten from the two INSERT statements"
        );
    }

    // The unify audit's finding 4. Do: 1,000 executes of one prepare_cached INSERT, and 1,000 runs
    // of one prepare_cached query, log search's next page as kiln's ci/logsearch.rs writes it.
    // Expect: each SQL text parsed once, at prepare, and every run of the query answering alike;
    // the same INSERT given as text while its statement is held (out of the cache, so unmarked)
    // parses again. The session's validate parsed the text again on every execute: 1,001 parses
    // each (AC-T1.1.1; docs/design/store.md, decisions 34 and 36).
    #[test]
    fn a_prepared_statement_is_parsed_once_however_often_it_runs() {
        use amphora_api::Value;
        const INSERT: &str = "INSERT INTO parsed_once(a, b) VALUES (?1, ?2)";
        const PAGE: &str = "SELECT p.finished,p.leg_order,p.leg,p.block,p.bits,
        (SELECT bits FROM ci_log_grams g WHERE g.gram=?2 AND g.finished=p.finished AND g.leg_order=p.leg_order AND g.leg=p.leg AND g.block=p.block),
        (SELECT bits FROM ci_log_grams g WHERE g.gram=?3 AND g.finished=p.finished AND g.leg_order=p.leg_order AND g.leg=p.leg AND g.block=p.block),
        (SELECT bits FROM ci_log_grams g WHERE g.gram=?4 AND g.finished=p.finished AND g.leg_order=p.leg_order AND g.leg=p.leg AND g.block=p.block)
        FROM ci_log_grams p WHERE p.gram=?1 AND (p.finished,p.leg_order,p.leg,p.block)<(?5,?6,?7,?8) ORDER BY p.finished DESC,p.leg_order DESC,p.leg DESC,p.block DESC LIMIT 8";
        let db = crate::database::Database::in_memory(until()).unwrap();
        let (pages, prepared, text) = db
            .call(until(), |c| {
                c.execute_batch(
                    "CREATE TABLE parsed_once(a INTEGER, b TEXT);
                     CREATE TABLE ci_log_grams(gram TEXT, finished INTEGER, leg_order INTEGER, leg TEXT, block INTEGER, bits BLOB);
                     INSERT INTO ci_log_grams VALUES ('abc', 3, 1, 'l1', 1, x'01'), ('bcd', 3, 1, 'l1', 1, x'02'),
                       ('abc', 3, 1, 'l1', 2, x'03'), ('cde', 3, 1, 'l1', 2, x'04'), ('abc', 2, 1, 'l0', 7, x'05')",
                )?;
                let mut insert = c.prepare_cached(INSERT)?;
                for i in 0..1000i64 {
                    insert.execute(vec![Value::Integer(i), Value::Text("x".into())])?;
                }
                let prepared = super::parses(INSERT);
                c.execute(INSERT, vec![Value::Integer(1000), Value::Text("x".into())])?;
                let text = super::parses(INSERT);
                let mut page = c.prepare_cached(PAGE)?;
                let cursor = || {
                    vec![
                        Value::Text("abc".into()),
                        Value::Text("bcd".into()),
                        Value::Text("cde".into()),
                        Value::Text("def".into()),
                        Value::Integer(9),
                        Value::Integer(9),
                        Value::Text("zz".into()),
                        Value::Integer(9),
                    ]
                };
                let mut pages = Vec::new();
                for _ in 0..1000 {
                    let blocks: Vec<i64> = page.query_map(cursor(), |r| r.get::<_, i64>(3))?.collect::<Result<_, _>>()?;
                    pages.push(blocks);
                }
                Ok((pages, prepared, text))
            })
            .unwrap();
        assert_eq!(
            pages[0],
            [2, 1, 7],
            "the next page: blocks 2 and 1 of leg l1, then block 7 of l0"
        );
        assert!(
            pages.iter().all(|blocks| *blocks == pages[0]),
            "every run answers alike"
        );
        assert_eq!(prepared, 1, "parses of the INSERT for 1,000 executes");
        assert_eq!(
            text, 2,
            "the same INSERT given as text while its statement is held parses again"
        );
        assert_eq!(
            super::parses(PAGE),
            1,
            "parses of the page query for 1,000 runs"
        );
    }

    // Finding 4's mark (Codex's spec, 2026-10-30; kiln goal 4320a144). Do: in one call, run an
    // INSERT as text three times, prepare a SELECT anew three times, and run two refused texts
    // twice each; in a second call, run the INSERT and prepare the SELECT again; then change the
    // schema and prepare the SELECT once more. Expect: the INSERT and the SELECT parsed once
    // across both calls, their entries marked by that validation; each refused text parsed both
    // times, never marked; the SELECT parsed again once the schema change dropped its entry
    // (AC-T1.1.1; docs/design/store.md, decision 36).
    #[test]
    fn a_validated_text_keeps_its_mark_until_its_cache_entry_goes() {
        use amphora_api::Value;
        const INSERT: &str = "INSERT INTO marked(a) VALUES (?1)";
        const SELECT: &str = "SELECT count(*) FROM marked WHERE a >= ?1";
        const CONTROL: &str = "BEGIN /* never marked */";
        const RESERVED: &str = "SELECT * FROM __amphora_versions /* never marked */";
        fn select(c: &crate::sql::Connection<'_>) -> crate::sql::Result<i64> {
            c.prepare_cached(SELECT)?
                .query_row(vec![Value::Integer(0)], |r| r.get(0))
        }
        let db = crate::database::Database::in_memory(until()).unwrap();
        let (texts, refusals) = db
            .call(until(), |c| {
                c.execute_batch("CREATE TABLE marked(a INTEGER)")?;
                for i in 0..3i64 {
                    c.execute(INSERT, vec![Value::Integer(i)])?;
                }
                for _ in 0..3 {
                    select(c)?;
                }
                let mut refusals = 0;
                for _ in 0..2 {
                    for refused in [CONTROL, RESERVED] {
                        refusals += usize::from(c.execute(refused, []).is_err());
                    }
                }
                Ok(([super::parses(INSERT), super::parses(SELECT)], refusals))
            })
            .unwrap();
        assert_eq!(
            texts,
            [1, 1],
            "the INSERT as text three times, the SELECT prepared three times"
        );
        assert_eq!(refusals, 4, "every refused text refused each time");
        assert_eq!(
            [super::parses(CONTROL), super::parses(RESERVED)],
            [2, 2],
            "a refused text is parsed again each time: it's never marked"
        );
        let (count, after) = db
            .call(until(), |c| {
                c.execute(INSERT, vec![Value::Integer(3)])?;
                let count = select(c)?;
                let again = [super::parses(INSERT), super::parses(SELECT)];
                c.execute_batch("CREATE TABLE marked_after(a INTEGER)")?;
                select(c)?;
                Ok((
                    count,
                    [again, [super::parses(INSERT), super::parses(SELECT)]],
                ))
            })
            .unwrap();
        assert_eq!(count, 4);
        assert_eq!(
            after,
            [[1, 1], [1, 2]],
            "marked across calls; the SELECT validated again after the schema change dropped its entry"
        );
    }

    // Finding 4's mark (kiln goal 4320a144). Do: run a text through Db's private path, then
    // prepare it twice through a session on the same Db; then prepare TX_MAX + 1 more texts and
    // the first of them again. Expect: the privately cached text validated on its first session
    // use (Db's private SQL never marks an entry) and not after, and the evicted text validated
    // again, its mark gone with its entry (AC-T1.1.1; docs/design/store.md, decision 36).
    #[test]
    fn a_privately_cached_or_evicted_text_is_validated_again() {
        const PRIVATE: &str = "SELECT 7 AS privately_cached";
        let db = crate::db::Db::open_with(
            std::path::Path::new("private-mark.db"),
            std::sync::Arc::new(amphora_engine::MemoryIO::new()),
        )
        .unwrap();
        db.query_owned(PRIVATE, [], |_| Ok(true)).unwrap();
        assert_eq!(
            super::parses(PRIVATE),
            0,
            "Db's private path doesn't validate"
        );
        let c = super::Connection::new(&db);
        let prepared = |sql: &str| -> i64 {
            c.prepare_cached(sql)
                .unwrap()
                .query_row([], |r| r.get(0))
                .unwrap()
        };
        for _ in 0..2 {
            assert_eq!(prepared(PRIVATE), 7);
        }
        assert_eq!(
            super::parses(PRIVATE),
            1,
            "validated on its first session use, then marked"
        );
        let text = |n: usize| format!("SELECT {n} AS evicted_{n}");
        for n in 0..=amphora_api::TX_MAX {
            prepared(&text(n));
        }
        prepared(&text(0));
        assert_eq!(
            super::parses(&text(0)),
            2,
            "evicted by TX_MAX newer texts, validated again"
        );
        assert_eq!(
            super::parses(&text(amphora_api::TX_MAX)),
            1,
            "the newest kept its mark"
        );
    }

    // Finding 4's mark (amphora-seam-owner, 2026-11-06, Codex's remaining concern). Do: run
    // `SAVEPOINT s` and `RELEASE s` twice each as Session text calls, which the session's
    // validation accepts and marks; then submit each text to Database::execute. Expect: each text
    // parsed once by the session, its entry marked; Database::execute still refuses both, by its
    // own validator: a Session mark never widens it (AC-T1.1.1; docs/design/store.md, decision 36).
    #[test]
    fn a_session_mark_never_widens_database_execute() {
        const SAVEPOINT: &str = "SAVEPOINT s /* marked by a session */";
        const RELEASE: &str = "RELEASE s /* marked by a session */";
        let db = crate::database::Database::in_memory(until()).unwrap();
        db.call(until(), |c| {
            for _ in 0..2 {
                c.execute(SAVEPOINT, [])?;
                c.execute(RELEASE, [])?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(
            [super::parses(SAVEPOINT), super::parses(RELEASE)],
            [1, 1],
            "the session validated each text once and marked its entry"
        );
        for sql in [SAVEPOINT, RELEASE] {
            let refused = db.execute(
                vec![amphora_api::Statement {
                    sql: sql.into(),
                    params: vec![],
                }],
                until(),
            );
            assert!(
                matches!(&refused, Err(Refusal::Invalid { what, why })
                    if what == "SQL" && why == "connection and transaction control belongs to Amphora"),
                "{sql}: {refused:?}"
            );
        }
    }
}
