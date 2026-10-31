//! U06/U15: fixed SQL for the shared host database. Table and index names are explicit;
//! statements pass unchanged to Amphora's SQL parser. `sql.rs` retains the historical schema
//! and queries for legacy fixtures. A sessions `meta` column is distinct from `convo_meta`.

/// Canonical schema versions match the versions already recorded by the Amphora adapter.
/// Keep shipped versions unchanged; append subsequent migrations here.
pub const MIGRATIONS: &[&str] = &[
    // 1: sessions, events and the harness's record.
    "CREATE TABLE IF NOT EXISTS convo_sessions (
        id TEXT PRIMARY KEY,
        info TEXT,
        meta TEXT,
        state TEXT,
        changes TEXT,
        updated_at INTEGER NOT NULL DEFAULT 0,
        closed_at INTEGER
    );
    CREATE TABLE IF NOT EXISTS convo_events (
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
    CREATE INDEX IF NOT EXISTS convo_events_by_seq ON convo_events (session, seq);
    CREATE INDEX IF NOT EXISTS convo_events_by_time ON convo_events (at);
    CREATE TABLE IF NOT EXISTS convo_records (
        session TEXT NOT NULL,
        n INTEGER NOT NULL,
        line TEXT NOT NULL,
        PRIMARY KEY (session, n)
    );",
    // 2: revisions, epochs, source files and historical feed metadata.
    "ALTER TABLE convo_sessions ADD COLUMN epoch INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE convo_sessions ADD COLUMN rev INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE convo_sessions ADD COLUMN machine TEXT;
    ALTER TABLE convo_events ADD COLUMN rev INTEGER NOT NULL DEFAULT 0;
    UPDATE convo_sessions SET rev = rowid * 2;
    UPDATE convo_events SET rev = rowid * 2 + 1;
    CREATE INDEX IF NOT EXISTS convo_sessions_by_rev ON convo_sessions (rev);
    CREATE INDEX IF NOT EXISTS convo_events_by_rev ON convo_events (rev);
    DROP TABLE IF EXISTS convo_records;
    CREATE TABLE convo_records (
        session TEXT NOT NULL,
        source TEXT NOT NULL,
        n INTEGER NOT NULL,
        line_end INTEGER NOT NULL,
        line TEXT NOT NULL,
        rev INTEGER NOT NULL,
        PRIMARY KEY (session, source, n)
    );
    CREATE INDEX convo_records_by_rev ON convo_records (rev);
    CREATE TABLE convo_drops (
        rev INTEGER PRIMARY KEY,
        session TEXT NOT NULL,
        what TEXT NOT NULL,
        source TEXT,
        n INTEGER,
        at INTEGER NOT NULL
    );
    CREATE TABLE convo_peers (
        peer TEXT PRIMARY KEY,
        rev INTEGER NOT NULL,
        at INTEGER NOT NULL
    );",
    // 3: historical copy metadata, retained by the read-only importer.
    "CREATE TABLE IF NOT EXISTS convo_copied (session TEXT PRIMARY KEY);",
    // 4: codec 2 segments and restartable import cursors.
    "CREATE TABLE IF NOT EXISTS convo_segments (
        session TEXT NOT NULL, source TEXT NOT NULL, n INTEGER NOT NULL,
        part INTEGER NOT NULL, value BLOB NOT NULL,
        PRIMARY KEY(session,source,n,part)
    );
    CREATE TABLE IF NOT EXISTS convo_import (
        source TEXT NOT NULL, table_name TEXT NOT NULL, cursor INTEGER NOT NULL,
        done INTEGER NOT NULL, fingerprint TEXT NOT NULL,
        PRIMARY KEY(source,table_name)
    );",
    // 5: immutable content staged before atomic codec 3 record publication.
    "CREATE TABLE IF NOT EXISTS convo_chunks (
        digest BLOB NOT NULL, part INTEGER NOT NULL, value BLOB NOT NULL,
        PRIMARY KEY(digest,part)
    );
    CREATE TABLE IF NOT EXISTS convo_chunk_manifests (
        digest BLOB PRIMARY KEY, size INTEGER NOT NULL, parts INTEGER NOT NULL,
        ready INTEGER NOT NULL
    );",
    // 6 (U06/U15/U19): source-local histories cannot share global rowids or feed revisions.
    // Payloads are postcard Vec<Value>; large UTF-8 values use immutable codec 3 references.
    "CREATE TABLE IF NOT EXISTS convo_legacy_rows (
        source TEXT NOT NULL, table_name TEXT NOT NULL, source_rowid INTEGER NOT NULL,
        session TEXT, item TEXT, number INTEGER, epoch INTEGER NOT NULL,
        source_rev INTEGER, payload BLOB NOT NULL,
        PRIMARY KEY(source,table_name,source_rowid)
    );
    CREATE INDEX IF NOT EXISTS convo_legacy_rows_by_key ON convo_legacy_rows(source,table_name,session,item,number);
    CREATE TABLE IF NOT EXISTS convo_legacy_resets (
        source TEXT NOT NULL, source_rowid INTEGER NOT NULL, session TEXT NOT NULL,
        what TEXT NOT NULL, record_source TEXT, n INTEGER, at INTEGER NOT NULL,
        epoch INTEGER NOT NULL, PRIMARY KEY(source,source_rowid)
    );
    CREATE INDEX IF NOT EXISTS convo_legacy_rows_by_foreign_key ON convo_legacy_rows(table_name,session,item,number,epoch,source);
    CREATE INDEX IF NOT EXISTS convo_legacy_resets_by_source ON convo_legacy_resets(source,session,what,record_source,n,epoch,at);
    CREATE INDEX IF NOT EXISTS convo_legacy_resets_by_session ON convo_legacy_resets(session,what,record_source,n,epoch,at,source);
    CREATE INDEX IF NOT EXISTS convo_drops_by_session ON convo_drops(session,what,source,n,at);
    CREATE INDEX IF NOT EXISTS convo_import_by_completion ON convo_import(table_name,done,source);",
    // 7 (U06/U15): full immutable-content reservations, including abandoned partial staging.
    // Seed once from manifest metadata in this migration's atomic schema transaction. Later
    // admissions update this singleton with their manifest; reopening never rescans or resets it.
    "CREATE TABLE IF NOT EXISTS convo_chunk_pool (
        id INTEGER PRIMARY KEY CHECK(id=1),
        bytes INTEGER NOT NULL CHECK(bytes>=0),
        manifests INTEGER NOT NULL CHECK(manifests>=0)
    );
    INSERT OR IGNORE INTO convo_chunk_pool(id,bytes,manifests)
        SELECT 1,coalesce(sum(size),0),count(*) FROM convo_chunk_manifests;",
];

pub const META: &str = "CREATE TABLE IF NOT EXISTS convo_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)";
pub const META_GET: &str = "SELECT value FROM convo_meta WHERE key = ?1";
pub const META_SET: &str = "INSERT INTO convo_meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value";
pub const REV_MAX: &[&str] = &["SELECT MAX(rev) FROM convo_sessions", "SELECT MAX(rev) FROM convo_events", "SELECT MAX(rev) FROM convo_records", "SELECT MAX(rev) FROM convo_drops"];

pub const SESSION_UPSERT: &str = "INSERT INTO convo_sessions (id, info, meta, state, changes, updated_at, epoch, rev) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
    ON CONFLICT (id) DO UPDATE SET
        info = COALESCE(excluded.info, convo_sessions.info),
        meta = COALESCE(excluded.meta, convo_sessions.meta),
        state = COALESCE(excluded.state, convo_sessions.state),
        changes = COALESCE(excluded.changes, convo_sessions.changes),
        updated_at = excluded.updated_at,
        epoch = MAX(excluded.epoch, convo_sessions.epoch),
        rev = excluded.rev";
pub const SESSION_OPEN: &str = "UPDATE convo_sessions SET closed_at = NULL WHERE id = ?1";
pub const SESSION_CLOSE: &str = "UPDATE convo_sessions SET closed_at = ?2, rev = ?3 WHERE id = ?1";
pub const SESSION_GET: &str = "SELECT info, meta, state, changes, updated_at, closed_at FROM convo_sessions WHERE id = ?1";
pub const SESSIONS_UNENDED: &str = "SELECT id, info, meta, state, machine FROM convo_sessions WHERE closed_at IS NULL AND info IS NOT NULL AND updated_at >= ?1 ORDER BY updated_at";
pub const SESSIONS_ENDED_AFTER: &str = "SELECT id, closed_at FROM convo_sessions WHERE closed_at IS NOT NULL AND (closed_at > ?1 OR (closed_at = ?1 AND id > ?2)) ORDER BY closed_at, id LIMIT ?3";
pub const SESSION_MACHINE: &str = "UPDATE convo_sessions SET machine = ?2 WHERE id = ?1";

pub const EVENT_UPSERT: &str = "INSERT INTO convo_events (session, id, seq, at, parent, by, kind, level, tool, status, text, body, rev) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
    ON CONFLICT (session, id) DO UPDATE SET
        seq = excluded.seq, at = excluded.at, parent = excluded.parent, by = excluded.by, kind = excluded.kind,
        level = excluded.level, tool = excluded.tool, status = excluded.status, text = excluded.text, body = excluded.body,
        rev = excluded.rev
    WHERE convo_events.seq IS NOT excluded.seq OR convo_events.at IS NOT excluded.at OR convo_events.parent IS NOT excluded.parent OR convo_events.by IS NOT excluded.by
        OR convo_events.kind IS NOT excluded.kind OR convo_events.level IS NOT excluded.level OR convo_events.tool IS NOT excluded.tool
        OR convo_events.status IS NOT excluded.status OR convo_events.text IS NOT excluded.text OR convo_events.body IS NOT excluded.body";
pub const EVENTS_RESET: &str = "DELETE FROM convo_events WHERE session = ?1 AND seq >= ?2";
pub const EVENTS_LAST: &str = "SELECT session, id, seq, at, parent, by, kind, level, tool, status, text, body FROM convo_events WHERE session = ?1 ORDER BY seq DESC LIMIT ?2";
pub const EVENTS_BEFORE: &str = "SELECT session, id, seq, at, parent, by, kind, level, tool, status, text, body FROM convo_events WHERE session = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3";
pub const SEARCH_ALL: &str = "SELECT rowid, at, at, kind, tool, status, text FROM convo_events WHERE at <= ?1 ORDER BY at DESC, rowid DESC";
pub const SEARCH_SESSION: &str = "SELECT rowid, seq, at, kind, tool, status, text FROM convo_events WHERE session = ?1 AND seq <= ?2 ORDER BY seq DESC, rowid DESC";
pub const EVENT_BY_ROWID: &str = "SELECT session, id, seq, at, parent, by, kind, level, tool, status, text, body FROM convo_events WHERE rowid = ?1";

pub const RECORD_UPSERT: &str = "INSERT INTO convo_records (session, source, n, line_end, line, rev) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
    ON CONFLICT (session, source, n) DO UPDATE SET line_end = excluded.line_end, line = excluded.line, rev = excluded.rev
    WHERE convo_records.line_end IS NOT excluded.line_end OR convo_records.line IS NOT excluded.line";
pub const RECORDS_RESET: &str = "DELETE FROM convo_records WHERE session = ?1 AND source = ?2";
pub const RECORD_SOURCES: &str = "SELECT source, MAX(n) FROM convo_records WHERE session = ?1 GROUP BY source";
pub const RECORD_LINE: &str = "SELECT line_end, line FROM convo_records WHERE session = ?1 AND source = ?2 AND n = ?3";
pub const RECORD_LINES: &str = "SELECT n, line FROM convo_records WHERE session = ?1 AND source = ?2 AND n >= ?3 ORDER BY n LIMIT ?4";
pub const ANY_SESSION: &str = "SELECT 1 FROM convo_sessions LIMIT 1";
pub const ANY_EVENT: &str = "SELECT 1 FROM convo_events LIMIT 1";
pub const COUNT_SESSIONS: &str = "SELECT count(*) FROM convo_sessions";
pub const DROP_ADD: &str = "INSERT INTO convo_drops (rev, session, what, source, n, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)";

// U05/U06/U15: fixed metadata/payload plans, never rewritten caller SQL.
// Numeric event keys permit a bounded ordered result; payload bytes are point-read.
pub(super) const KEY_PAGE: usize = 256;
pub(super) struct EventPage {
    pub first: String,
    pub equal: String,
    pub lower: String,
    pub payload: &'static str,
    pub session: bool,
    pub limit: Option<usize>,
}
pub(super) fn event_page(query: &str) -> Option<EventPage> {
    let (filter,key,index,parameters,payload,session,limit)=match query {
        EVENTS_LAST=>("session=?1","seq","convo_events_by_seq",2,EVENT_BY_ROWID,true,Some(1)),
        EVENTS_BEFORE=>("session=?1 AND seq<?2","seq","convo_events_by_seq",3,EVENT_BY_ROWID,true,Some(2)),
        SEARCH_SESSION=>("session=?1 AND seq<=?2","seq","convo_events_by_seq",2,"SELECT rowid,seq,at,kind,tool,status,text FROM convo_events WHERE rowid=?1",true,None),
        SEARCH_ALL=>("at<=?1","at","convo_events_by_time",1,"SELECT rowid,at,at,kind,tool,status,text FROM convo_events WHERE rowid=?1",false,None),
        _=>return None,
    };
    let select=format!("SELECT rowid,{key} FROM convo_events INDEXED BY {index}");
    let first=format!("{select} WHERE {filter} ORDER BY {key} DESC,rowid DESC LIMIT ?{}",parameters+1);
    let (prefix,k,r,n)=if session {("session=?1 AND ",2,3,4)}else{("",1,2,3)};
    // U05/U06/U15: requires the coordinated native implicit-rowid suffix fix.
    // Equal prefix + rowid range must be a full physical secondary-index seek,
    // not a residual filter. Never combine these probes into tuple/OR predicates.
    let equal=format!("{select} WHERE {prefix}{key}=?{k} AND rowid<?{r} ORDER BY rowid DESC LIMIT ?{n}");
    let lower=format!("{select} WHERE {prefix}{key}<?{k} ORDER BY {key} DESC,rowid DESC LIMIT ?{}",r);
    Some(EventPage {first,equal,lower,payload,session,limit})
}
pub(super) struct Inventory {
    pub scan: String,
    pub payload: &'static str,
    pub limit: Option<usize>,
}
pub(super) fn inventory(query: &str) -> Option<Inventory> {
    let (filter,key,payload,limit,parameters)=match query {
        SESSIONS_UNENDED=>("closed_at IS NULL AND info IS NOT NULL AND updated_at>=?1","updated_at","SELECT id,info,meta,state,machine FROM convo_sessions WHERE rowid=?1",None,1),
        SESSIONS_ENDED_AFTER=>("closed_at IS NOT NULL AND (closed_at>?1 OR (closed_at=?1 AND id>?2))","closed_at","SELECT id,closed_at FROM convo_sessions WHERE rowid=?1",Some(2),3),
        _=>return None,
    };
    let limit_filter=limit.map_or(String::new(),|i|format!(" AND ?{} IS NOT NULL",i+1));
    // One variable-sized ID per owned result; scalar table-rowid scan, sort once.
    let scan=format!("SELECT rowid,{key},id FROM convo_sessions NOT INDEXED WHERE ({filter}){limit_filter} AND rowid>?{} ORDER BY rowid LIMIT 1",parameters+1);
    Some(Inventory {scan,payload,limit})
}
pub(super) struct Iteration {
    pub first: String,
    pub next: String,
    pub keys: &'static [usize],
    pub limit: Option<usize>,
    pub page: usize,
}
pub(super) fn iteration(query: &str) -> Option<Iteration> {
    let (select,filter,order,cursor,keys,limit,page):
        (&str,&str,&str,&str,&'static [usize],Option<usize>,usize)=match query {
        RECORD_LINES=>("n,line","session=?1 AND source=?2 AND n>=?3","n","n>?5",&[0],Some(3),8),
        RECORD_SOURCES=>("source,MAX(n)","session=?1","source","source>?2",&[0],None,1),
        _=>return None,
    };
    let group=if query==RECORD_SOURCES {" GROUP BY source"}else{""};
    let bound=limit.map_or_else(||page.to_string(),|i|format!("CASE WHEN ?{n} BETWEEN 0 AND {page} THEN ?{n} ELSE {page} END",n=i+1));
    let first=format!("SELECT {select} FROM convo_records WHERE {filter}{group} ORDER BY {order} LIMIT {bound}");
    let next=format!("SELECT {select} FROM convo_records WHERE {filter} AND {cursor}{group} ORDER BY {order} LIMIT {bound}");
    Some(Iteration {first,next,keys,limit,page})
}
