//! Devin's native conversations, read from its database without changing its files. Only the
//! main branch's visible messages leave the reader; system context and sealed metadata do not.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::io::Read as _;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, params_from_iter};

use super::{Entry, PastSession, clip, entry, first_line};

const BATCH: usize = 200;
const PAGE: usize = 64;
const CANDIDATES: usize = 4096;
const DEPTH: usize = 4096;
const MESSAGES: usize = 150;
const TEXT: usize = 2001;
const CANDIDATE_QUERY: &str = "SELECT id, hidden, last_activity_at FROM sessions ORDER BY last_activity_at DESC LIMIT ?1";

#[derive(Clone, Debug)]
pub(super) struct Record {
    pub id: String,
    pub cwd: String,
    pub title: String,
    pub model: String,
    pub created_at: i64,
    pub last_activity: i64,
    /// The latest message of each visible role, oldest first, for the scanner's summary.
    pub entries: Vec<Entry>,
}

fn database(root: &Path) -> Option<Connection> {
    let file = root.join("sessions.db");
    if !file.is_file() {
        return None;
    }
    let db = Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
    db.busy_timeout(Duration::from_millis(50)).ok()?;
    Some(db)
}

/// Summaries of sessions identified by their live lock records, in one bounded query.
pub(super) fn records(root: &Path, ids: &[String]) -> Vec<Record> {
    let Some(db) = database(root) else { return Vec::new() };
    read(&db, ids, 1, true).unwrap_or_default()
}

/// Recently active past conversations, with the same keeping and running exclusions as the
/// other native harnesses. Pages count all rows, including hidden sessions, to bound the scan.
pub(super) fn history(root: &Path, limit: usize, skip: &HashSet<String>) -> Vec<PastSession> {
    let Some(db) = database(root) else { return Vec::new() };
    let limit = limit.clamp(1, BATCH);
    let mut out = Vec::new();
    let mut candidates = candidates(&db).unwrap_or_default();
    candidates.sort_unstable_by(|a, b| b.2.cmp(&a.2).then_with(|| b.0.cmp(&a.0)));
    for page in candidates.chunks(PAGE) {
        if out.len() == limit {
            break;
        }
        let ids: Vec<String> = page.iter().filter(|(id, hidden, _)| !hidden && !skip.contains(id)).map(|(id, _, _)| id.clone()).collect();
        let mut records: HashMap<String, Record> = read(&db, &ids, 1, true).unwrap_or_default().into_iter().map(|r| (r.id.clone(), r)).collect();
        for (id, _, _) in page {
            let Some(r) = records.remove(id) else { continue };
            let prompt = r.entries.iter().rev().find(|e| e.role == "user").map(|e| e.text.as_str()).unwrap_or_default();
            if prompt.is_empty() && r.title.is_empty() {
                continue;
            }
            out.push(PastSession {
                kind: "devin".into(),
                id: r.id,
                title: if r.title.is_empty() { first_line(prompt, 80) } else { r.title },
                cwd: r.cwd,
                model: r.model,
                last_activity: r.last_activity,
                last_prompt: clip(prompt, super::SNIPPET),
                ..PastSession::default()
            });
            if out.len() == limit {
                break;
            }
        }
    }
    out
}

fn candidates(db: &Connection) -> rusqlite::Result<Vec<(String, bool, i64)>> {
    let mut q = db.prepare(CANDIDATE_QUERY)?;
    q.query_map([i64::try_from(CANDIDATES).unwrap_or(0)], |r| Ok((r.get(0)?, r.get::<_, Option<bool>>(1)?.unwrap_or(false), r.get::<_, Option<i64>>(2)?.unwrap_or(0))))?.collect()
}

/// Recent messages from the selected main branch. Alternative branches and subagents are never
/// merged into the conversation, even when they have newer node ids.
pub(super) fn transcript(root: &Path, id: &str, limit: usize) -> Option<Vec<Entry>> {
    let db = database(root)?;
    read(&db, &[id.to_owned()], limit.clamp(1, MESSAGES), false).ok()?.into_iter().next().map(|r| r.entries)
}

fn read(db: &Connection, ids: &[String], limit: usize, summary: bool) -> rusqlite::Result<Vec<Record>> {
    let ids: Vec<&String> = ids.iter().take(BATCH).collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let slots = std::iter::repeat_n("?", ids.len()).collect::<Vec<_>>().join(",");
    let rank = if summary { "session_id, role" } else { "session_id" };
    let query = format!(
        "WITH RECURSIVE selected AS (
            SELECT id, substr(COALESCE(working_directory, ''), 1, 4096) AS cwd, substr(COALESCE(title, ''), 1, 400) AS title,
                   substr(COALESCE(model, ''), 1, 512) AS model, COALESCE(created_at, 0) AS created_at,
                   COALESCE(last_activity_at, 0) AS last_activity_at, main_chain_id
            FROM sessions WHERE id IN ({slots}) AND COALESCE(hidden, 0) = 0
        ), chain(session_id, node_id, parent_node_id, depth) AS (
            SELECT s.id, n.node_id, n.parent_node_id, 0 FROM selected s
            JOIN message_nodes n ON n.session_id = s.id AND n.node_id = s.main_chain_id
            UNION ALL
            SELECT c.session_id, n.node_id, n.parent_node_id, c.depth + 1 FROM chain c
            JOIN message_nodes n ON n.session_id = c.session_id AND n.node_id = c.parent_node_id
            WHERE c.depth < {DEPTH}
        ), decoded AS (
            SELECT c.session_id, c.depth,
                   CASE WHEN json_valid(n.chat_message) THEN n.chat_message ELSE '{{}}' END AS message
            FROM chain c JOIN message_nodes n ON n.session_id = c.session_id AND n.node_id = c.node_id
        ), visible AS (
            SELECT session_id, depth, json_extract(message, '$.role') AS role,
                   substr(json_extract(message, '$.content'), 1, {TEXT}) AS text,
                   substr(COALESCE(json_extract(message, '$.metadata.created_at'), ''), 1, 80) AS at
            FROM decoded
            WHERE json_extract(message, '$.role') IN ('user', 'assistant', 'tool')
              AND json_type(message, '$.content') = 'text'
              AND (json_extract(message, '$.role') != 'user' OR COALESCE(json_extract(message, '$.metadata.is_user_input'), 1) != 0)
        ), ranked AS (
            SELECT *, ROW_NUMBER() OVER (PARTITION BY {rank} ORDER BY depth) AS recent FROM visible
            WHERE length(trim(text)) > 0
        )
        SELECT s.id, s.cwd, s.title, s.model, s.created_at, s.last_activity_at, r.role, r.text, r.at
        FROM selected s LEFT JOIN ranked r ON r.session_id = s.id AND r.recent <= {limit}
        ORDER BY s.id, r.depth DESC"
    );
    let mut q = db.prepare(&query)?;
    let mut rows = q.query(params_from_iter(ids))?;
    let mut out: Vec<Record> = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        if out.last().is_none_or(|r| r.id != id) {
            out.push(Record { id, cwd: row.get(1)?, title: row.get(2)?, model: row.get(3)?, created_at: row.get(4)?, last_activity: row.get(5)?, entries: Vec::new() });
        }
        if let (Some(role), Some(text)) = (row.get::<_, Option<String>>(6)?, row.get::<_, Option<String>>(7)?)
            && let Some(record) = out.last_mut()
        {
            let at: String = row.get(8)?;
            record.entries.push(entry(&role, &text, &at));
        }
    }
    Ok(out)
}

/// Held locks identify live owners; recent acquisitions take priority over a profile's stale files.
/// Several sessions may belong to one ACP process, so none overwrites another.
pub(super) fn locks(root: &Path) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut newest = BinaryHeap::new();
    for file in std::fs::read_dir(root.join("session_locks")).into_iter().flatten().flatten() {
        let path = file.path();
        if !path.extension().is_some_and(|e| e == "lock") {
            continue;
        }
        let Ok(meta) = file.metadata() else { continue };
        let Ok(modified) = meta.modified() else { continue };
        if !meta.is_file() {
            continue;
        }
        newest.push(Reverse((modified, path)));
        if newest.len() > CANDIDATES {
            newest.pop();
        }
    }
    for Reverse((_, path)) in newest.into_sorted_vec() {
        let Some(id) = path.file_stem().and_then(|s| s.to_str()).filter(|s| !s.is_empty()).map(str::to_owned) else { continue };
        let Ok(file) = std::fs::File::open(path) else { continue };
        if !matches!(file.try_lock_shared(), Err(std::fs::TryLockError::WouldBlock)) {
            continue;
        }
        let mut pid = String::new();
        if (&file).take(32).read_to_string(&mut pid).is_ok()
            && let Ok(pid) = pid.trim().parse::<u32>()
            && pid > 1
        {
            out.push((pid, id));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use serde_json::json;

    fn setup() -> anyhow::Result<(tempfile::TempDir, Connection)> {
        let dir = tempfile::tempdir()?;
        let db = Connection::open(dir.path().join("sessions.db"))?;
        db.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY, working_directory TEXT, model TEXT, title TEXT, created_at INTEGER, last_activity_at INTEGER, main_chain_id INTEGER, hidden INTEGER DEFAULT 0); CREATE INDEX idx_sessions_activity ON sessions(last_activity_at DESC); CREATE TABLE message_nodes (session_id TEXT, node_id INTEGER, parent_node_id INTEGER, chat_message TEXT, UNIQUE(session_id, node_id));")?;
        Ok((dir, db))
    }

    fn session(db: &Connection, id: &str, at: i64, head: i64, hidden: bool) -> anyhow::Result<()> {
        db.execute("INSERT INTO sessions VALUES (?1, '/work', 'model', '', 1, ?2, ?3, ?4)", params![id, at, head, hidden])?;
        Ok(())
    }

    fn node(db: &Connection, id: &str, n: i64, parent: Option<i64>, role: &str, text: &str) -> anyhow::Result<()> {
        let message = json!({"role": role, "content": text, "metadata": {"created_at": "2026-10-22T12:00:00Z"}});
        db.execute("INSERT INTO message_nodes VALUES (?1, ?2, ?3, ?4)", params![id, n, parent, message.to_string()])?;
        Ok(())
    }

    #[test]
    fn missing_databases_and_unknown_schemas_are_not_created_or_changed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert!(records(dir.path(), &["s1".into()]).is_empty());
        assert!(history(dir.path(), 10, &HashSet::new()).is_empty());
        assert!(transcript(dir.path(), "s1", 10).is_none());
        assert!(!dir.path().join("sessions.db").exists());
        let db = Connection::open(dir.path().join("sessions.db"))?;
        db.execute_batch("CREATE TABLE other (id TEXT)")?;
        assert!(records(dir.path(), &["s1".into()]).is_empty());
        assert!(history(dir.path(), 10, &HashSet::new()).is_empty());
        Ok(())
    }

    #[test]
    fn only_the_selected_main_chain_is_read_and_summaries_keep_each_last_role() -> anyhow::Result<()> {
        let (dir, db) = setup()?;
        session(&db, "s1", 10, 3, false)?;
        node(&db, "s1", 1, None, "user", "question")?;
        node(&db, "s1", 2, Some(1), "assistant", "answer")?;
        node(&db, "s1", 3, Some(2), "tool", "result")?;
        node(&db, "s1", 4, Some(1), "assistant", "other branch")?;
        node(&db, "s1", 5, None, "assistant", "subagent")?;
        let rows = records(dir.path(), &["s1".into()]);
        let r = rows.first().ok_or_else(|| anyhow::anyhow!("no session"))?;
        assert_eq!((&r.id, r.cwd.as_str(), r.model.as_str(), r.created_at, r.last_activity), (&"s1".to_owned(), "/work", "model", 1, 10));
        assert_eq!(r.entries.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), ["question", "answer", "result"]);
        let entries = transcript(dir.path(), "s1", 2).ok_or_else(|| anyhow::anyhow!("no transcript"))?;
        assert_eq!(entries.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), ["answer", "result"]);
        Ok(())
    }

    #[test]
    fn history_pages_past_hidden_and_running_sessions_and_uses_latest_activity() -> anyhow::Result<()> {
        let (dir, db) = setup()?;
        for n in 0..(PAGE + 2) {
            let id = format!("s{n:03}");
            session(&db, &id, i64::try_from(n).unwrap_or(0) + 1, 1, n >= 2)?;
            node(&db, &id, 1, None, "user", &id)?;
        }
        let past = history(dir.path(), 2, &HashSet::from(["s001".into()]));
        assert_eq!(past.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["s000"]);
        assert_eq!(past.first().map(|p| (p.kind.as_str(), p.last_prompt.as_str())), Some(("devin", "s000")));
        assert!(records(dir.path(), &["s002".into()]).is_empty());
        assert!(transcript(dir.path(), "s002", 10).is_none());
        Ok(())
    }

    #[test]
    fn candidates_use_the_activity_index_and_metadata_nulls_do_not_drop_other_records() -> anyhow::Result<()> {
        let (dir, db) = setup()?;
        db.execute_batch("CREATE INDEX idx_sessions_hidden ON sessions(hidden); INSERT INTO sessions VALUES ('empty', NULL, NULL, NULL, NULL, NULL, NULL, NULL)")?;
        session(&db, "s1", 10, 1, false)?;
        node(&db, "s1", 1, None, "user", "question")?;
        let mut q = db.prepare(&format!("EXPLAIN QUERY PLAN {CANDIDATE_QUERY}"))?;
        let plan: Vec<String> = q.query_map([64], |r| r.get(3))?.collect::<rusqlite::Result<_>>()?;
        assert!(plan.iter().any(|p| p.contains("idx_sessions_activity")));
        assert!(plan.iter().all(|p| !p.contains("TEMP B-TREE")));
        let rows = records(dir.path(), &["empty".into(), "s1".into()]);
        assert_eq!(rows.len(), 2);
        let empty = rows.iter().find(|r| r.id == "empty").ok_or_else(|| anyhow::anyhow!("no empty session"))?;
        assert!(empty.cwd.is_empty() && empty.model.is_empty() && empty.entries.is_empty());
        assert_eq!((empty.created_at, empty.last_activity), (0, 0));
        assert_eq!(history(dir.path(), 1, &HashSet::new()).first().map(|p| p.id.as_str()), Some("s1"));
        Ok(())
    }

    #[test]
    fn messages_and_chain_depth_are_bounded_and_private_or_malformed_messages_are_skipped() -> anyhow::Result<()> {
        let (dir, db) = setup()?;
        session(&db, "s1", 10, 6, false)?;
        node(&db, "s1", 1, None, "system", "private system context")?;
        node(&db, "s1", 2, Some(1), "user", "<strong>visible prompt</strong>")?;
        let fake = json!({"role": "user", "content": "internal prompt", "metadata": {"is_user_input": false}});
        db.execute("INSERT INTO message_nodes VALUES ('s1', 3, 2, ?1)", [fake.to_string()])?;
        db.execute("INSERT INTO message_nodes VALUES ('s1', 4, 3, 'not json')", [])?;
        let sealed = json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "sealed", "signature": "private"}]} );
        db.execute("INSERT INTO message_nodes VALUES ('s1', 5, 4, ?1)", [sealed.to_string()])?;
        node(&db, "s1", 6, Some(5), "assistant", &"x".repeat(TEXT * 10))?;
        let entries = transcript(dir.path(), "s1", usize::MAX).ok_or_else(|| anyhow::anyhow!("no transcript"))?;
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.text == "<strong>visible prompt</strong>"));
        assert!(entries.iter().all(|e| e.text.chars().count() <= super::super::ENTRY_CHARS + 1));
        session(&db, "cycle", 11, 1, false)?;
        node(&db, "cycle", 1, Some(1), "user", "loop")?;
        let entries = transcript(dir.path(), "cycle", usize::MAX).ok_or_else(|| anyhow::anyhow!("no cyclic transcript"))?;
        assert_eq!(entries.len(), MESSAGES);
        Ok(())
    }

    #[test]
    fn lock_files_identify_only_held_owners_and_preserve_multiple_sessions() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert!(locks(dir.path()).is_empty());
        std::fs::create_dir(dir.path().join("session_locks"))?;
        let lock = |id: &str, pid: &str| -> anyhow::Result<std::fs::File> {
            let path = dir.path().join("session_locks").join(format!("{id}.lock"));
            std::fs::write(&path, pid)?;
            let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
            file.lock()?;
            Ok(file)
        };
        let first = lock("first", "1234")?;
        let second = lock("second", "1234")?;
        let malformed = lock("bad", "not a pid")?;
        std::fs::write(dir.path().join("session_locks/stale.lock"), "9999")?;
        let mut owners = locks(dir.path());
        owners.sort();
        assert_eq!(owners, [(1234, "first".into()), (1234, "second".into())]);
        first.unlock()?;
        second.unlock()?;
        malformed.unlock()?;
        drop((first, second, malformed));
        assert!(locks(dir.path()).is_empty());
        Ok(())
    }

    #[test]
    fn recently_acquired_locks_take_priority_over_a_profile_of_stale_files() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("session_locks");
        std::fs::create_dir(&root)?;
        let set_time = |path: &Path, seconds| -> anyhow::Result<()> {
            let time = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
            std::fs::OpenOptions::new().write(true).open(path)?.set_times(std::fs::FileTimes::new().set_modified(time))?;
            Ok(())
        };
        let oldest = root.join("oldest.lock");
        std::fs::write(&oldest, "4321")?;
        let old_lock = std::fs::OpenOptions::new().read(true).write(true).open(&oldest)?;
        old_lock.lock()?;
        set_time(&oldest, 1)?;
        for n in 0..=CANDIDATES {
            let path = root.join(format!("stale-{n}.lock"));
            std::fs::write(&path, "9999")?;
            set_time(&path, 2)?;
        }
        let recent = root.join("recent.lock");
        std::fs::write(&recent, "1234")?;
        let recent_lock = std::fs::OpenOptions::new().read(true).write(true).open(&recent)?;
        recent_lock.lock()?;
        set_time(&recent, 3)?;
        assert_eq!(locks(dir.path()), [(1234, "recent".into())]);
        drop((old_lock, recent_lock));
        Ok(())
    }
}
