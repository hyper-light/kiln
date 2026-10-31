//! U05/CP-T1.1: one bounded prepared-statement cache for static and dynamic SQL. Checking a
//! statement out removes it until the caller returns it, so stepping and nested row callbacks
//! never hold the cache mutex. Returning a statement makes it most recently used; eviction takes
//! the least recently used. Oversized entries execute normally and are simply not retained.

use amphora_engine::{Statement, schema::Schema};
use std::collections::{HashMap, VecDeque};
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

#[derive(Default)]
struct State {
    statements: HashMap<Arc<str>, Statement>,
    order: VecDeque<Arc<str>>,
    sql_bytes: usize,
    schema: Option<(Arc<Schema>, u32)>,
}
impl State {
    fn remove(&mut self, sql: &str) -> Result<Option<Statement>> {
        let Some((key, statement)) = self.statements.remove_entry(sql) else {
            return Ok(None);
        };
        let position = self
            .order
            .iter()
            .position(|held| Arc::ptr_eq(held, &key))
            .ok_or(Error::Accounting)?;
        self.order.remove(position).ok_or(Error::Accounting)?;
        self.sql_bytes = self
            .sql_bytes
            .checked_sub(key.len())
            .ok_or(Error::Accounting)?;
        Ok(Some(statement))
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

    pub(super) fn take(&self, sql: &str) -> Result<Option<Statement>> {
        self.locked()?.remove(sql)
    }

    /// Only successfully executed/prepared statements are returned here. Reset before taking
    /// the lock, and clear parameters so cached statements retain no request values or blobs.
    pub(super) fn put(&self, sql: &str, mut statement: Statement) -> Result<()> {
        statement.reset().map_err(|e| Error::Reset(e.to_string()))?;
        statement.clear_bindings();
        if self.limits.entries == 0 || sql.len() > self.limits.sql_bytes {
            return Ok(());
        }
        let mut retired = Vec::new();
        {
            let mut state = self.locked()?;
            if let Some(prior) = state.remove(sql)? {
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
                    let key: Arc<str> = Arc::from(sql);
                    state.sql_bytes = bytes;
                    state.order.push_back(Arc::clone(&key));
                    if let Some(previous) = state.statements.insert(key, statement) {
                        retired.push(previous);
                    }
                    admitted = true;
                    break;
                }
                let oldest = state.order.front().cloned().ok_or(Error::Accounting)?;
                retired.push(state.remove(&oldest)?.ok_or(Error::Accounting)?);
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
        cache.put("SELECT ?1", statement)?;
        let mut statement = cache
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
        cache.put("SELECT ?1", statement)?;
        cache.put("SELECT 2", db.conn.prepare("SELECT 2")?)?;
        cache.put("SELECT 3", db.conn.prepare("SELECT 3")?)?;
        assert!(
            cache.take("SELECT ?1")?.is_none(),
            "least recently used entry was evicted"
        );
        let statement = cache
            .take("SELECT 2")?
            .ok_or_else(|| anyhow::anyhow!("second statement missing"))?;
        cache.put("SELECT 2", statement)?;
        cache.put("SELECT 4", db.conn.prepare("SELECT 4")?)?;
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
        bytes.put("SELECT 2", db.conn.prepare("SELECT 2")?)?;
        bytes.put("SELECT 3", db.conn.prepare("SELECT 3")?)?;
        bytes.put("SELECT 12345", db.conn.prepare("SELECT 12345")?)?;
        assert!(
            bytes.take("SELECT 2")?.is_none() && bytes.take("SELECT 3")?.is_none(),
            "SQL byte budget evicts while entry slots remain"
        );
        assert!(bytes.take("SELECT 12345")?.is_some());
        Ok(())
    }
}
