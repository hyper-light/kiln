//! U05/CP-T1.1: one bounded prepared-statement cache for static and dynamic SQL. Checking a
//! statement out removes it until the caller returns it, so stepping and nested row callbacks
//! never hold the cache mutex, and a statement in use is never handed out twice. It keeps at most
//! `amphora_api::TX_MAX` statements and `amphora_api::SQL_MAX` bytes of their text. Returning a
//! statement makes it most recently used; eviction takes the least recently used, until both
//! budgets hold. Oversized entries execute normally and are simply not retained. The recency order
//! is a list linked through the map, so taking, returning and evicting a statement each cost a few
//! map operations, never a search of the order (docs/design/store.md, decision 34). An entry also
//! carries whether the session's validation accepted its text: only the session sets that mark,
//! and it goes with the entry when the entry is evicted, replaced or dropped for a schema change
//! or a restore (decision 36).

use amphora_engine::{Statement, schema::Schema};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("the prepared-statement cache lock was poisoned")]
    Poisoned,
    #[error("the prepared-statement cache accounting is inconsistent")]
    Accounting,
    #[error("resetting a prepared statement failed: {0}")]
    Reset(String),
}
type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy)]
struct Limits {
    entries: usize,
    sql_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        // Administrative bounds, not a throughput tuning claim: one API batch's statement
        // count and one maximum SQL request's text retained per connection (U05).
        Self {
            entries: amphora_api::TX_MAX,
            sql_bytes: amphora_api::SQL_MAX,
        }
    }
}

/// A kept statement, whether the session's validation accepted its text, and its neighbours in
/// the recency order, named by their text.
struct Entry {
    statement: Statement,
    validated: bool,
    older: Option<Arc<str>>,
    newer: Option<Arc<str>>,
}

#[derive(Default)]
struct State {
    statements: HashMap<Arc<str>, Entry>,
    oldest: Option<Arc<str>>,
    newest: Option<Arc<str>>,
    sql_bytes: usize,
    schema: Option<(Arc<Schema>, u32)>,
}
impl State {
    /// Takes `sql`'s statement out with its mark, its neighbours joined where it was.
    fn remove(&mut self, sql: &str) -> Result<Option<(Statement, bool)>> {
        let Some((key, entry)) = self.statements.remove_entry(sql) else {
            return Ok(None);
        };
        match &entry.older {
            Some(older) => {
                self.statements
                    .get_mut(older)
                    .ok_or(Error::Accounting)?
                    .newer
                    .clone_from(&entry.newer);
            }
            None => self.oldest.clone_from(&entry.newer),
        }
        match &entry.newer {
            Some(newer) => {
                self.statements
                    .get_mut(newer)
                    .ok_or(Error::Accounting)?
                    .older
                    .clone_from(&entry.older);
            }
            None => self.newest.clone_from(&entry.older),
        }
        self.sql_bytes = self
            .sql_bytes
            .checked_sub(key.len())
            .ok_or(Error::Accounting)?;
        Ok(Some((entry.statement, entry.validated)))
    }

    /// Keeps `statement` as the most recently used, its text making `sql_bytes` in all: the
    /// caller has checked both budgets and taken out any statement kept for the same text.
    fn push(
        &mut self,
        key: Arc<str>,
        statement: Statement,
        validated: bool,
        sql_bytes: usize,
    ) -> Result<()> {
        match &self.newest {
            Some(newest) => {
                self.statements
                    .get_mut(newest)
                    .ok_or(Error::Accounting)?
                    .newer = Some(Arc::clone(&key));
            }
            None => self.oldest = Some(Arc::clone(&key)),
        }
        let entry = Entry {
            statement,
            validated,
            older: self.newest.replace(Arc::clone(&key)),
            newer: None,
        };
        if self.statements.insert(key, entry).is_some() {
            return Err(Error::Accounting);
        }
        self.sql_bytes = sql_bytes;
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct Cache {
    state: Mutex<State>,
    limits: Limits,
}
impl Cache {
    fn locked(&self) -> Result<MutexGuard<'_, State>> {
        self.state.lock().map_err(|_| Error::Poisoned)
    }

    /// Keep the schema Arc, rather than its address alone, so allocator reuse cannot make a
    /// stale schema look current. The cookie also fences any in-place metadata update.
    pub(super) fn schema(&self, current: Arc<Schema>) -> Result<()> {
        let previous = {
            let mut state = self.locked()?;
            if state.schema.as_ref().is_some_and(|(held, version)| {
                Arc::ptr_eq(held, &current) && *version == current.schema_version
            }) {
                return Ok(());
            }
            let previous = std::mem::take(&mut *state);
            let version = current.schema_version;
            state.schema = Some((current, version));
            previous
        };
        drop(previous);
        Ok(())
    }

    /// Takes `sql`'s statement out, with whether the session's validation accepted its text.
    pub(super) fn take(&self, sql: &str) -> Result<Option<(Statement, bool)>> {
        self.locked()?.remove(sql)
    }

    /// Only successfully executed/prepared statements are returned here. Reset before taking
    /// the lock, and clear parameters so cached statements retain no request values or blobs.
    /// `validated` says the session's validation accepted the text; nothing else may set it.
    pub(super) fn put(&self, sql: &str, mut statement: Statement, validated: bool) -> Result<()> {
        statement.reset().map_err(|e| Error::Reset(e.to_string()))?;
        statement.clear_bindings();
        if self.limits.entries == 0 || sql.len() > self.limits.sql_bytes {
            return Ok(());
        }
        let mut retired = Vec::new();
        {
            let mut state = self.locked()?;
            if let Some((prior, _)) = state.remove(sql)? {
                retired.push(prior);
            }
            // Both budgets are administrative and finite. At most the entry budget's number
            // of evictions can occur; the extra iteration admits the new statement afterward.
            let attempts = self
                .limits
                .entries
                .checked_add(1)
                .ok_or(Error::Accounting)?;
            let mut admitted = false;
            for _ in 0..attempts {
                let bytes = state
                    .sql_bytes
                    .checked_add(sql.len())
                    .ok_or(Error::Accounting)?;
                if state.statements.len() < self.limits.entries && bytes <= self.limits.sql_bytes {
                    state.push(Arc::from(sql), statement, validated, bytes)?;
                    admitted = true;
                    break;
                }
                let oldest = state.oldest.clone().ok_or(Error::Accounting)?;
                retired.push(state.remove(&oldest)?.ok_or(Error::Accounting)?.0);
            }
            if !admitted {
                return Err(Error::Accounting);
            }
        }
        // Finalization can touch the engine; no engine work happens while holding the mutex.
        drop(retired);
        Ok(())
    }

    pub(super) fn clear(&self) -> Result<()> {
        let previous = {
            let mut state = self.locked()?;
            std::mem::take(&mut *state)
        };
        drop(previous);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amphora_engine::{StepResult, Value};
    use std::num::NonZero;
    use std::path::Path;

    #[test]
    fn u05_cached_statements_release_parameters_and_evict_by_both_budgets() -> anyhow::Result<()> {
        let db = crate::db::Db::open_with(
            Path::new("cache-test.db"),
            Arc::new(amphora_engine::MemoryIO::new()),
        )?;
        let cache = Cache {
            state: Mutex::default(),
            limits: Limits {
                entries: 2,
                sql_bytes: 32,
            },
        };
        let mut statement = db.conn.prepare("SELECT ?1")?;
        statement.bind_at(
            NonZero::new(1).ok_or_else(|| anyhow::anyhow!("parameter index"))?,
            Value::from_text("prior request".to_owned()),
        )?;
        cache.put("SELECT ?1", statement, false)?;
        let (mut statement, _) = cache
            .take("SELECT ?1")?
            .ok_or_else(|| anyhow::anyhow!("cached statement missing"))?;
        assert!(matches!(statement.step()?, StepResult::Row));
        assert!(
            matches!(
                statement.row().map(|row| row.get_value(0)),
                Some(Value::Null)
            ),
            "request values are cleared before retention"
        );
        cache.put("SELECT ?1", statement, false)?;
        cache.put("SELECT 2", db.conn.prepare("SELECT 2")?, false)?;
        cache.put("SELECT 3", db.conn.prepare("SELECT 3")?, false)?;
        assert!(
            cache.take("SELECT ?1")?.is_none(),
            "least recently used entry was evicted"
        );
        let (statement, _) = cache
            .take("SELECT 2")?
            .ok_or_else(|| anyhow::anyhow!("second statement missing"))?;
        cache.put("SELECT 2", statement, false)?;
        cache.put("SELECT 4", db.conn.prepare("SELECT 4")?, false)?;
        assert!(
            cache.take("SELECT 3")?.is_none(),
            "checkout and return refreshes recency"
        );
        assert!(cache.take("SELECT 2")?.is_some());
        cache.clear()?;
        assert!(cache.take("SELECT 4")?.is_none());
        let bytes = Cache {
            state: Mutex::default(),
            limits: Limits {
                entries: 3,
                sql_bytes: 16,
            },
        };
        bytes.put("SELECT 2", db.conn.prepare("SELECT 2")?, false)?;
        bytes.put("SELECT 3", db.conn.prepare("SELECT 3")?, false)?;
        bytes.put("SELECT 12345", db.conn.prepare("SELECT 12345")?, false)?;
        assert!(
            bytes.take("SELECT 2")?.is_none() && bytes.take("SELECT 3")?.is_none(),
            "SQL byte budget evicts while entry slots remain"
        );
        assert!(bytes.take("SELECT 12345")?.is_some());
        Ok(())
    }

    // Kiln plan 01a18d12142c7789a40e959ccdf52bd1, step 2. Do: fill a cache at its own bounds with
    // TX_MAX + 1 statements, taking and returning the first before the last goes in. Expect: the
    // second-oldest evicted, the first kept, TX_MAX held (AC-T1.1.1; store.md, decision 34).
    #[test]
    fn a_full_cache_evicts_its_least_recently_used_statement() -> anyhow::Result<()> {
        let db = crate::db::Db::open_with(
            Path::new("lru-test.db"),
            Arc::new(amphora_engine::MemoryIO::new()),
        )?;
        let cache = Cache::default();
        let max = amphora_api::TX_MAX;
        for n in 0..max {
            let sql = format!("SELECT {n}");
            cache.put(&sql, db.conn.prepare(&sql)?, false)?;
        }
        let (first, _) = cache
            .take("SELECT 0")?
            .ok_or_else(|| anyhow::anyhow!("the first statement is missing"))?;
        cache.put("SELECT 0", first, false)?;
        let last = format!("SELECT {max}");
        cache.put(&last, db.conn.prepare(&last)?, false)?;
        assert_eq!(
            cache.locked()?.statements.len(),
            max,
            "the cache holds its bound"
        );
        assert!(
            cache.take("SELECT 1")?.is_none(),
            "the least recently used, the second-oldest, went"
        );
        assert!(
            cache.take("SELECT 0")?.is_some(),
            "the first, taken and returned, stayed"
        );
        assert!(cache.take(&last)?.is_some());
        Ok(())
    }
}
