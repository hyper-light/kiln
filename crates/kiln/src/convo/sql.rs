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
    // 2: revisions (a store numbers its writes; a feed sends what came after a number), epochs
    // (a session's writer), the machine a feed brought a session from, the harness's record by
    // the file it is from, what was dropped (a feed sends that too), and the peers a store takes
    // feeds from. Rows from before get numbers of their own: sessions even, events odd.
    "ALTER TABLE sessions ADD COLUMN epoch INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE sessions ADD COLUMN rev INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE sessions ADD COLUMN machine TEXT;
    ALTER TABLE events ADD COLUMN rev INTEGER NOT NULL DEFAULT 0;
    UPDATE sessions SET rev = rowid * 2;
    UPDATE events SET rev = rowid * 2 + 1;
    CREATE INDEX IF NOT EXISTS sessions_by_rev ON sessions (rev);
    CREATE INDEX IF NOT EXISTS events_by_rev ON events (rev);
    DROP TABLE IF EXISTS records;
    CREATE TABLE records (
        session TEXT NOT NULL,
        source TEXT NOT NULL,
        n INTEGER NOT NULL,
        line_end INTEGER NOT NULL,
        line TEXT NOT NULL,
        rev INTEGER NOT NULL,
        PRIMARY KEY (session, source, n)
    );
    CREATE INDEX records_by_rev ON records (rev);
    CREATE TABLE drops (
        rev INTEGER PRIMARY KEY,
        session TEXT NOT NULL,
        what TEXT NOT NULL,
        source TEXT,
        n INTEGER,
        at INTEGER NOT NULL
    );
    CREATE TABLE peers (
        peer TEXT PRIMARY KEY,
        rev INTEGER NOT NULL,
        at INTEGER NOT NULL
    );",
    // 3: the sessions a mirror's copy brought back into this store while it restores from it:
    // the copy fills those in, and leaves alone what the store has from anywhere else.
    "CREATE TABLE IF NOT EXISTS copied (session TEXT PRIMARY KEY);",
];

/// Where the migrations done so far are counted.
pub const META: &str = "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)";
pub const SCHEMA_GET: &str = "SELECT value FROM meta WHERE key = 'schema'";
pub const SCHEMA_SET: &str = "INSERT INTO meta (key, value) VALUES ('schema', ?1) ON CONFLICT (key) DO UPDATE SET value = excluded.value";

/// A store's own values: `rev` (the last number it gave a write), `id` (who it is to a peer).
pub const META_GET: &str = "SELECT value FROM meta WHERE key = ?1";
pub const META_SET: &str = "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value";
/// The highest number each table gave, for a store opened again.
pub const REV_MAX: &[&str] = &["SELECT MAX(rev) FROM sessions", "SELECT MAX(rev) FROM events", "SELECT MAX(rev) FROM records", "SELECT MAX(rev) FROM drops"];

/// A session's parts: each one given replaces what was kept (NULL: unchanged). Its epoch only
/// rises.
pub const SESSION_UPSERT: &str = "INSERT INTO sessions (id, info, meta, state, changes, updated_at, epoch, rev) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
    ON CONFLICT (id) DO UPDATE SET
        info = COALESCE(excluded.info, sessions.info),
        meta = COALESCE(excluded.meta, sessions.meta),
        state = COALESCE(excluded.state, sessions.state),
        changes = COALESCE(excluded.changes, sessions.changes),
        updated_at = excluded.updated_at,
        epoch = MAX(excluded.epoch, sessions.epoch),
        rev = excluded.rev";
/// It (re)started: it isn't closed.
pub const SESSION_OPEN: &str = "UPDATE sessions SET closed_at = NULL WHERE id = ?1";
pub const SESSION_CLOSE: &str = "UPDATE sessions SET closed_at = ?2, rev = ?3 WHERE id = ?1";
pub const SESSION_GET: &str = "SELECT info, meta, state, changes, updated_at, closed_at FROM sessions WHERE id = ?1";
pub const SESSION_EPOCH: &str = "SELECT epoch FROM sessions WHERE id = ?1";
/// Sessions not ended, heard of since `?1` (unix ms).
pub const SESSIONS_UNENDED: &str = "SELECT id, info, meta, state, machine FROM sessions WHERE closed_at IS NULL AND info IS NOT NULL AND updated_at >= ?1 ORDER BY updated_at";
/// The machine whose feed brought the session.
pub const SESSION_MACHINE: &str = "UPDATE sessions SET machine = ?2 WHERE id = ?1";

pub const EVENT_UPSERT: &str = "INSERT INTO events (session, id, seq, at, parent, by, kind, level, tool, status, text, body, rev) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
    ON CONFLICT (session, id) DO UPDATE SET
        seq = excluded.seq, at = excluded.at, parent = excluded.parent, by = excluded.by, kind = excluded.kind,
        level = excluded.level, tool = excluded.tool, status = excluded.status, text = excluded.text, body = excluded.body,
        rev = excluded.rev";
/// A session's events from number `?2` on (a reset: `?2` is 0 for all of them).
pub const EVENTS_RESET: &str = "DELETE FROM events WHERE session = ?1 AND seq >= ?2";
/// A session's last `?2` events, newest first (the caller turns them around).
pub const EVENTS_LAST: &str = "SELECT session, id, seq, at, parent, by, kind, level, tool, status, text, body FROM events WHERE session = ?1 ORDER BY seq DESC LIMIT ?2";
/// A session's `?3` events before event number `?2`, newest first.
pub const EVENTS_BEFORE: &str = "SELECT session, id, seq, at, parent, by, kind, level, tool, status, text, body FROM events WHERE session = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3";

/// The columns an event row is read from, in this order.
pub const EVENT_COLUMNS: usize = 12;
/// A search looks at events newest first by an index, a slice at a time: across every agent by
/// time (`events_by_time`), from `?1` back; in one agent's by number (`events_by_seq`), from `?2`
/// back. Each row: its rowid, the key it goes in order of, its time, kind, tool, status, text.
pub const SEARCH_ALL: &str = "SELECT rowid, at, at, kind, tool, status, text FROM events WHERE at <= ?1 ORDER BY at DESC, rowid DESC";
pub const SEARCH_SESSION: &str = "SELECT rowid, seq, at, kind, tool, status, text FROM events WHERE session = ?1 AND seq <= ?2 ORDER BY seq DESC, rowid DESC";
/// An event a search found, whole.
pub const EVENT_BY_ROWID: &str = "SELECT session, id, seq, at, parent, by, kind, level, tool, status, text, body FROM events WHERE rowid = ?1";

pub const RECORD_UPSERT: &str = "INSERT INTO records (session, source, n, line_end, line, rev) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
    ON CONFLICT (session, source, n) DO UPDATE SET line_end = excluded.line_end, line = excluded.line, rev = excluded.rev";
pub const RECORDS_RESET: &str = "DELETE FROM records WHERE session = ?1 AND source = ?2";
/// Each file of a session's record, with its last line kept: where following it goes on.
pub const RECORD_SOURCES: &str = "SELECT source, MAX(n) FROM records WHERE session = ?1 GROUP BY source";
pub const RECORD_LINE: &str = "SELECT line_end, line FROM records WHERE session = ?1 AND source = ?2 AND n = ?3";
/// `?4` lines of a session's record from `source`, from line `?3` on, in order.
pub const RECORD_LINES: &str = "SELECT n, line FROM records WHERE session = ?1 AND source = ?2 AND n >= ?3 ORDER BY n LIMIT ?4";
/// `?2` record lines written before number `?1`, newest first: what compressing the lines kept
/// before lines were compressed walks through, down the numbers.
pub const RECORDS_BELOW: &str = "SELECT session, source, n, line, rev FROM records WHERE rev < ?1 ORDER BY rev DESC LIMIT ?2";
/// A line, compressed: the same line, so its number for a feed stays as it was.
pub const RECORD_SQUEEZE: &str = "UPDATE records SET line = ?4 WHERE session = ?1 AND source = ?2 AND n = ?3";
/// The sessions that ended before `?1` (unix ms): their records go after `keep_closed_days`.
pub const SESSIONS_ENDED_BEFORE: &str = "SELECT id FROM sessions WHERE closed_at IS NOT NULL AND closed_at < ?1";
/// The first file of a session's record still kept.
pub const RECORD_FIRST_SOURCE: &str = "SELECT source FROM records WHERE session = ?1 ORDER BY source, n LIMIT 1";
/// The numbers of the first `?3` lines kept of a file of a session's record.
pub const RECORD_FIRST_LINES: &str = "SELECT n FROM records WHERE session = ?1 AND source = ?2 ORDER BY n LIMIT ?3";
/// A file of a session's record let go through line `?3`. No drop is written: every store lets
/// an ended session's record go by its own clock, so no feed carries it.
pub const RECORDS_PRUNE: &str = "DELETE FROM records WHERE session = ?1 AND source = ?2 AND n <= ?3";

/// A copy restoring this store (`copied`): the sessions it brought, and whether one is its.
pub const COPIED_HAS: &str = "SELECT 1 FROM copied WHERE session = ?1";
pub const COPIED_ADD: &str = "INSERT INTO copied (session) VALUES (?1) ON CONFLICT (session) DO NOTHING";
pub const COPIED_DROP: &str = "DELETE FROM copied WHERE session = ?1";
pub const COPIED_ANY: &str = "SELECT 1 FROM copied LIMIT 1";
pub const COPIED_COUNT: &str = "SELECT count(*) FROM copied";
pub const COPIED_CLEAR: &str = "DELETE FROM copied";
/// Whether the store has anything of a session: its row, an event, a line of its record.
pub const KNOWN_SESSION: &str = "SELECT 1 FROM sessions WHERE id = ?1 LIMIT 1";
pub const KNOWN_EVENTS: &str = "SELECT 1 FROM events WHERE session = ?1 LIMIT 1";
pub const KNOWN_RECORDS: &str = "SELECT 1 FROM records WHERE session = ?1 LIMIT 1";
/// Whether the store holds any session at all.
pub const ANY_SESSION: &str = "SELECT 1 FROM sessions LIMIT 1";
pub const ANY_EVENT: &str = "SELECT 1 FROM events LIMIT 1";

/// What was dropped, numbered as the writes are (each one kept: a feed sends them in order).
pub const DROP_ADD: &str = "INSERT INTO drops (rev, session, what, source, n, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)";

/// A feed: what came after a number, oldest first, `?2` at most of each kind.
pub const FEED_SESSIONS: &str = "SELECT id, info, meta, state, changes, updated_at, closed_at, epoch, rev, machine FROM sessions WHERE rev > ?1 ORDER BY rev LIMIT ?2";
pub const FEED_EVENTS: &str = "SELECT session, id, seq, at, parent, by, kind, level, tool, status, text, body, rev FROM events WHERE rev > ?1 ORDER BY rev LIMIT ?2";
pub const FEED_RECORDS: &str = "SELECT session, source, n, line_end, line, rev FROM records WHERE rev > ?1 ORDER BY rev LIMIT ?2";
pub const FEED_DROPS: &str = "SELECT rev, session, what, source, n, at FROM drops WHERE rev > ?1 ORDER BY rev LIMIT ?2";

/// Through where a store took a peer's feed.
pub const PEER_GET: &str = "SELECT rev FROM peers WHERE peer = ?1";
/// Every peer whose feed a store took, through where, and when it last took from it.
pub const PEERS_ALL: &str = "SELECT peer, rev, at FROM peers ORDER BY peer";
pub const PEER_SET: &str = "INSERT INTO peers (peer, rev, at) VALUES (?1, ?2, ?3) ON CONFLICT (peer) DO UPDATE SET rev = excluded.rev, at = excluded.at";
