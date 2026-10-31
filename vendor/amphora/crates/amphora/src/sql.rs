//! U05: Amphora's implementation of its public SQL session API. The database owns the
//! transaction and durable acknowledgement; the session cannot commit it independently.

use crate::db::Db;
use amphora_api::Value;
pub use amphora_api::sql::*;
use amphora_engine::{Numeric, Value as Native};
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

fn validate(sql: &str, batch: bool) -> Result<()> {
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
    fn query(&self, sql: &str, params: Vec<Value>, first: bool) -> Result<Vec<Vec<Value>>> {
        validate(sql, false)?;
        let params = params.into_iter().map(native).collect::<Result<Vec<_>>>()?;
        let mut rows = Vec::new();
        let mut bytes = 0usize;
        self.query_owned(sql, params, |row| {
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
    fn column_count(&self, sql: &str) -> Result<usize> {
        validate(sql, false)?;
        Db::column_count(self, sql).map_err(failed)
    }
    fn changes(&self) -> i64 {
        Db::changes(self)
    }
    fn last_insert_rowid(&self) -> i64 {
        Db::last_insert_rowid(self)
    }
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
}
