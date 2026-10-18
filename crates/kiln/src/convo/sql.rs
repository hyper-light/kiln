//! The store's schema and statements, as SQL any SQLite-compatible engine runs (Turso on a
//! machine). Migrations go by number: each runs once, in order, recorded in `meta`.

/// The migrations, in order. Never edit one that shipped: add the next.
pub const MIGRATIONS: &[&str] = &[
    // 1: sessions, events, the harness's record.
    "CREATE TABLE IF NOT EXISTS sessions (
        id TEXT PRIMARY KEY,
        info TEXT,
        meta TEXT,
        state TEXT,
        changes TEXT,
        updated_at INTEGER NOT NULL DEFAULT 0,
        closed_at INTEGER
    );
    CREATE TABLE IF NOT EXISTS events (
        session TEXT NOT NULL,
        id TEXT NOT NULL,
        seq INTEGER NOT NULL,
        at INTEGER NOT NULL,
        parent TEXT,
        by TEXT,
        kind TEXT NOT NULL,
        level TEXT,
        tool TEXT,
        status TEXT,
        text TEXT NOT NULL,
        body TEXT NOT NULL,
        PRIMARY KEY (session, id)
    );
    CREATE INDEX IF NOT EXISTS events_by_seq ON events (session, seq);
    CREATE INDEX IF NOT EXISTS events_by_time ON events (at);
    CREATE TABLE IF NOT EXISTS records (
        session TEXT NOT NULL,
        n INTEGER NOT NULL,
        line TEXT NOT NULL,
        PRIMARY KEY (session, n)
    );",
];

/// Where the migrations done so far are counted.
pub const META: &str = "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)";
pub const SCHEMA_GET: &str = "SELECT value FROM meta WHERE key = 'schema'";
pub const SCHEMA_SET: &str = "INSERT INTO meta (key, value) VALUES ('schema', ?1) ON CONFLICT (key) DO UPDATE SET value = excluded.value";

/// A session's parts: each one given replaces what was kept (NULL: unchanged).
pub const SESSION_UPSERT: &str = "INSERT INTO sessions (id, info, meta, state, changes, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
    ON CONFLICT (id) DO UPDATE SET
        info = COALESCE(excluded.info, sessions.info),
        meta = COALESCE(excluded.meta, sessions.meta),
        state = COALESCE(excluded.state, sessions.state),
        changes = COALESCE(excluded.changes, sessions.changes),
        updated_at = excluded.updated_at";
pub const SESSION_CLOSE: &str = "UPDATE sessions SET closed_at = ?2 WHERE id = ?1";
pub const SESSION_GET: &str = "SELECT info, meta, state, changes, updated_at, closed_at FROM sessions WHERE id = ?1";

pub const EVENT_UPSERT: &str = "INSERT INTO events (session, id, seq, at, parent, by, kind, level, tool, status, text, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
    ON CONFLICT (session, id) DO UPDATE SET
        seq = excluded.seq, at = excluded.at, parent = excluded.parent, by = excluded.by, kind = excluded.kind,
        level = excluded.level, tool = excluded.tool, status = excluded.status, text = excluded.text, body = excluded.body";
pub const EVENTS_RESET: &str = "DELETE FROM events WHERE session = ?1";
/// A session's last `?2` events, newest first (the caller turns them around).
pub const EVENTS_LAST: &str = "SELECT session, id, seq, at, parent, by, kind, level, tool, status, text, body FROM events WHERE session = ?1 ORDER BY seq DESC LIMIT ?2";
/// A session's `?3` events before event number `?2`, newest first.
pub const EVENTS_BEFORE: &str = "SELECT session, id, seq, at, parent, by, kind, level, tool, status, text, body FROM events WHERE session = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3";

/// The columns an event row is read from, in this order.
pub const EVENT_COLUMNS: usize = 12;
