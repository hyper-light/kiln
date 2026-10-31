//! An exclusive bootstrap owner. Partial imports cannot be opened through Database.
use super::*;
use amphora_api::import::ImportProgress;
use amphora_api::restore::RestorePage;

pub(super) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS __amphora_restore (id INTEGER PRIMARY KEY CHECK(id = 1), source TEXT NOT NULL, cursor BLOB NOT NULL, done INTEGER NOT NULL)";
const CURSOR_MAX: usize = 4096;

/// The only handle exposed while SQL pages are restored with foreign keys temporarily disabled.
/// Completion validates every FK before returning the normal, enforcing Database handle.
pub struct Restore {
    database: Database,
    source: String,
}

fn progress(db: &Db) -> anyhow::Result<Option<ImportProgress>> {
    let mut state = None;
    db.query(
        "SELECT source, cursor, done FROM __amphora_restore WHERE id = 1",
        [],
        |r| {
            state = Some(ImportProgress {
                source: r.get::<String>(0).map_err(unavailable)?,
                cursor: bytes(r, 1)?.ok_or_else(|| anyhow::anyhow!("restore cursor is null"))?,
                done: r.get::<i64>(2).map_err(unavailable)? != 0,
            });
            Ok(false)
        },
    )?;
    Ok(state)
}

impl Database {
    pub fn restore_progress(&self, until: Instant) -> Result<Option<ImportProgress>, Refusal> {
        let database = self.resolve(until)?;
        if let Some(client) = database.remote_client() {
            return client.restore_progress(until).map_err(Refusal::from);
        }
        database.call_db(until, |db| progress(db))
    }
}
pub(super) fn incomplete(db: &Db) -> anyhow::Result<bool> {
    Ok(progress(db)?.is_some_and(|p| !p.done))
}
fn fence(db: &Db, source: &str, cursor: &[u8]) -> anyhow::Result<ImportProgress> {
    let state = progress(db)?.ok_or_else(|| api::invalid("restore", "no restore was started"))?;
    if state.source != source || state.cursor != cursor {
        return Err(api::invalid("restore", "source identity or cursor changed").into());
    }
    Ok(state)
}

impl Restore {
    pub fn open(path: &Path, source: &str, until: Instant) -> Result<Self, Refusal> {
        if source.is_empty() {
            return Err(api::invalid("restore source", "empty"));
        }
        api::bound("restore source", source.len(), 256)?;
        let database = Database::open_owned(path, until, true, |db| {
            db.exec(super::SCHEMA).map_err(unavailable)?;
            db.exec(SCHEMA).map_err(unavailable)?;
            db.transaction(|db| {
            if let Some(prior) = progress(db)? {
                if prior.source != source {
                    return Err(api::invalid("restore source", "the destination belongs to another snapshot").into());
                }
            } else {
                let mut occupied = false;
                db.query("SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name NOT GLOB '__amphora_*' AND name NOT GLOB 'sqlite_*' LIMIT 1", [], |_| { occupied = true; Ok(false) })?;
                db.query("SELECT 1 FROM __amphora_versions LIMIT 1", [], |_| { occupied = true; Ok(false) })?;
                if occupied { return Err(api::invalid("restore destination", "it already holds application tables").into()); }
                db.query("INSERT INTO __amphora_restore(id, source, cursor, done) VALUES (1, ?1, ?2, 0)", [text(source), blob(&[])?], |_| Ok(true))?;
            }
            Ok(())
        }).map_err(|e| refused(&e))?;
            db.exec("PRAGMA foreign_keys = OFF").map_err(unavailable)?;
            Ok(())
        })?;
        Ok(Self {
            database,
            source: source.into(),
        })
    }

    pub fn progress(&self, until: Instant) -> Result<ImportProgress, Refusal> {
        self.database.call_db(until, |db| {
            progress(db)?.ok_or_else(|| anyhow::anyhow!("restore metadata missing"))
        })
    }

    pub fn apply_page(&self, page: RestorePage, until: Instant) -> Result<(), Refusal> {
        api::bound("restore cursor", page.expected_cursor.len(), CURSOR_MAX)?;
        api::bound("restore cursor", page.next_cursor.len(), CURSOR_MAX)?;
        if page.next_cursor == page.expected_cursor {
            return Err(api::invalid("restore cursor", "a page must advance"));
        }
        api::bound("restore statements", page.statements.len(), api::TX_MAX)?;
        let mut size = 0usize;
        if let Some(schema) = &page.schema {
            validate_sql(schema, true)?;
            size = schema.len();
        }
        for statement in &page.statements {
            validate_sql(&statement.sql, false)?;
            api::bound("restore parameters", statement.params.len(), api::TX_MAX)?;
            size = size
                .checked_add(statement.sql.len())
                .ok_or_else(|| api::invalid("restore page", "size overflow"))?;
            for value in &statement.params {
                size = size
                    .checked_add(value_bytes(value))
                    .ok_or_else(|| api::invalid("restore page", "size overflow"))?;
            }
        }
        api::bound("restore page bytes", size, api::REQUEST_MAX)?;
        let source = self.source.clone();
        self.database.call_db(until, move |db| {
            if fence(db, &source, &page.expected_cursor)?.done {
                return Err(api::invalid("restore", "already complete").into());
            }
            if let Some(schema) = &page.schema {
                db.exec(schema)?;
            }
            for statement in page.statements {
                let params = statement
                    .params
                    .iter()
                    .map(native)
                    .collect::<anyhow::Result<Vec<_>>>()?;
                db.query_owned(&statement.sql, params, |_| Ok(true))?;
            }
            db.query(
                "UPDATE __amphora_restore SET cursor = ?1 WHERE id = 1",
                [blob(&page.next_cursor)?],
                |_| Ok(true),
            )?;
            Ok(())
        })
    }

    pub fn finish(self, expected_cursor: Vec<u8>, until: Instant) -> Result<Database, Refusal> {
        let source = self.source.clone();
        self.database.call_db(until, move |db| {
            fence(db, &source, &expected_cursor)?;
            db.validate_foreign_keys()?;
            db.query(
                "UPDATE __amphora_restore SET done = 1 WHERE id = 1",
                [],
                |_| Ok(true),
            )?;
            Ok(())
        })?;
        let Writer::Local(store) = &self.database.shared.writer else {
            return Err(api::invalid(
                "restore",
                "requires an exclusive local bootstrap owner",
            ));
        };
        store
            .run(until, |local| {
                local.db.exec("PRAGMA foreign_keys = ON")?;
                local.restoring = false;
                Ok(())
            })
            .map_err(|e| refused(&e))?;
        self.database
            .shared
            .restoring
            .store(false, Ordering::Release);
        if let Some(path) = self.database.shared.path.get() {
            remote::publish(&self.database, path)?;
        }
        Ok(self.database)
    }
}
