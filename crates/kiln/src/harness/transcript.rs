//! Claude Code's transcripts (`<config>/projects/<dir>/<session>.jsonl`) as the timeline: the
//! history of a session kiln resumes, and the view of sessions running elsewhere (in a terminal).
//! Records are turned into the stream-json shape the Claude adapter reads, so both paths render
//! the same way. Only the conversation's own branch is kept (several processes can write to one
//! transcript): the chain of parent links back from its newest message.

use std::collections::{HashMap, HashSet};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::claude::Claude;
use super::core::Core;
use super::model::{Body, ToolStatus};

/// Bytes read from the end of a transcript for history.
pub const HISTORY_BYTES: u64 = 8 << 20;

/// Claude Code's configuration directory: `CLAUDE_CONFIG_DIR` (from `env`, else this process),
/// else `~/.claude`.
pub fn config_dir(env: &[(String, String)]) -> PathBuf {
    env.iter()
        .find(|(k, _)| k == "CLAUDE_CONFIG_DIR")
        .map(|(_, v)| PathBuf::from(v))
        .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from))
        .unwrap_or_else(|| crate::os::home_dir().join(".claude"))
}

/// The transcript of `session` under `config`, wherever its project directory is.
pub fn find(config: &Path, session: &str) -> Option<PathBuf> {
    if session.is_empty() || session.contains('/') || session.contains("..") {
        return None;
    }
    let name = format!("{session}.jsonl");
    std::fs::read_dir(config.join("projects")).ok()?.flatten().map(|e| e.path().join(&name)).find(|p| p.is_file())
}

/// The last `bytes` of `session`'s transcript under `config`, as whole lines.
pub fn tail(config: &Path, session: &str, bytes: u64) -> Option<Vec<String>> {
    tail_lines(&find(config, session)?, bytes)
}

/// The last `bytes` of a file as whole lines.
fn tail_lines(path: &Path, bytes: u64) -> Option<Vec<String>> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(bytes);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(bytes).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // cut mid-record
    }
    Some(lines)
}

/// The conversation's records, oldest first: the parent chain back from the newest message
/// (across compactions), so records other processes interleaved are left out.
fn branch(records: Vec<Value>, started: Option<i64>, upto: Option<&str>) -> Vec<Value> {
    let keep: HashSet<String> = {
        let msgs: Vec<&Value> = records.iter().filter(|r| r.get("uuid").and_then(Value::as_str).is_some()).collect();
        let by_id: HashMap<&str, &Value> = msgs.iter().filter_map(|r| Some((r.get("uuid")?.as_str()?, *r))).collect();
        let is_msg = |r: &&&Value| matches!(r.get("type").and_then(Value::as_str), Some("user" | "assistant"));
        // Each process that wrote here stamps its records with its own `session_id` (a lane).
        // The lane of the process started at `started` is the first to appear after it.
        let lane = started.and_then(|t| {
            let mut first: HashMap<&str, i64> = HashMap::new();
            for r in &msgs {
                if let (Some(l), Some(ts)) = (r.get("session_id").and_then(Value::as_str), r.get("timestamp").and_then(Value::as_str).and_then(unix_secs)) {
                    first.entry(l).or_insert(ts);
                }
            }
            first.into_iter().filter(|(_, ts)| *ts >= t - 5).min_by_key(|(_, ts)| *ts).map(|(l, _)| l)
        });
        let head = match (upto, lane) {
            (Some(u), _) => msgs.iter().find(|r| r.get("uuid").and_then(Value::as_str) == Some(u)),
            (None, Some(l)) => msgs.iter().rev().filter(is_msg).find(|r| r.get("session_id").and_then(Value::as_str) == Some(l)),
            (None, None) => msgs.iter().rev().find(is_msg),
        };
        let Some(last) = head else { return Vec::new() };
        let mut keep: HashSet<String> = HashSet::new();
        let mut at = Some(*last);
        while let Some(r) = at {
            let Some(id) = r.get("uuid").and_then(Value::as_str) else { break };
            if !keep.insert(id.to_owned()) {
                break; // a loop: stop
            }
            let parent = r.get("parentUuid").and_then(Value::as_str).or_else(|| r.get("logicalParentUuid").and_then(Value::as_str));
            at = parent.and_then(|p| by_id.get(p).copied());
        }
        keep
    };
    records.into_iter().filter(|r| r.get("uuid").and_then(Value::as_str).is_some_and(|u| keep.contains(u))).collect()
}

fn unix_secs(ts: &str) -> Option<i64> {
    time::OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339).ok().map(time::OffsetDateTime::unix_timestamp)
}

/// A transcript record in the stream-json shape the Claude adapter reads (None: not shown).
fn as_stream(r: &Value) -> Option<Value> {
    if r.get("isSidechain").and_then(Value::as_bool) == Some(true) || r.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let uuid = r.get("uuid").cloned().unwrap_or(Value::Null);
    match r.get("type").and_then(Value::as_str)? {
        "user" => {
            let human = r.pointer("/origin/kind").and_then(Value::as_str) == Some("human") || r.get("promptSource").and_then(Value::as_str) == Some("typed");
            let mut v = json!({"type": "user", "message": r.get("message")?, "uuid": uuid, "parent_tool_use_id": null});
            if let Some(o) = v.as_object_mut() {
                if let Some(t) = r.get("toolUseResult") {
                    o.insert("tool_use_result".into(), t.clone());
                }
                if human {
                    o.insert("isReplay".into(), json!(true));
                }
            }
            Some(v)
        }
        "assistant" => {
            let mut v = json!({"type": "assistant", "message": r.get("message")?, "uuid": uuid, "parent_tool_use_id": null});
            if r.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true)
                && let Some(o) = v.as_object_mut()
            {
                o.insert("isApiErrorMessage".into(), json!(true));
            }
            Some(v)
        }
        "system" if r.get("subtype").and_then(Value::as_str) == Some("compact_boundary") => {
            Some(json!({"type": "system", "subtype": "compact_boundary", "compact_metadata": r.get("compactMetadata").cloned().unwrap_or(Value::Null)}))
        }
        _ => None,
    }
}

/// Render a transcript's conversation into `core` (history: nothing is left running).
pub fn load(core: &mut Core, lines: &[String]) {
    load_for(core, lines, None);
}

/// [`load`], only up to (and including) the entry `upto` (a branch taken from that point).
pub fn load_upto(core: &mut Core, lines: &[String], upto: Option<&str>) {
    load_with(core, lines, None, upto);
}

/// [`load`], for the conversation of the process started at `started` (unix seconds), when
/// several processes write to one transcript.
pub fn load_for(core: &mut Core, lines: &[String], started: Option<i64>) {
    load_with(core, lines, started, None);
}

fn load_with(core: &mut Core, lines: &[String], started: Option<i64>, upto: Option<&str>) {
    let records: Vec<Value> = lines.iter().filter_map(|l| serde_json::from_str(l).ok()).collect();
    // The title Claude Code gave the session, if any.
    if let Some(t) = records.iter().rev().find_map(|r| (r.get("type").and_then(Value::as_str) == Some("ai-title")).then(|| r.get("aiTitle").and_then(Value::as_str)).flatten()) {
        core.state.title = t.to_owned();
    }
    let mut adapter = Claude::new(false);
    for r in branch(records, started, upto) {
        if let Some(v) = as_stream(&r) {
            adapter.line(core, &v.to_string());
        }
    }
    // History is history: calls without a result were cut off, and nothing is in progress.
    let open: Vec<String> = core
        .events()
        .filter(|e| matches!(&e.body, Body::Tool(t) if matches!(t.status, ToolStatus::Running | ToolStatus::Pending)))
        .map(|e| e.id.clone())
        .collect();
    for id in open {
        if let Some(e) = core.get_mut(&id)
            && let Body::Tool(t) = &mut e.body
        {
            t.status = ToolStatus::Cancelled;
        }
    }
    for id in core.events().filter(|e| matches!(e.body, Body::Assistant { streaming: true, .. } | Body::Thinking { streaming: true, .. })).map(|e| e.id.clone()).collect::<Vec<_>>() {
        if let Some(e) = core.get_mut(&id) {
            match &mut e.body {
                Body::Assistant { streaming, .. } | Body::Thinking { streaming, .. } => *streaming = false,
                _ => {}
            }
        }
    }
    core.state.approvals.clear();
    // Not live yet: the harness that resumes it says when it is ready.
    core.state.status = super::model::Status::Starting;
    core.state.turn_started = None;
    core.state.doing = None;
    core.state.activity = None;
    core.state.delivery.clear();
    core.state.usage = Default::default();
    core.take_writes();
    core.previews.clear();
    core.touch_state();
}

/// The history of Claude Code session `session` (blocking: reads the transcript).
pub fn history(env: &[(String, String)], session: &str) -> Option<Vec<String>> {
    let file = find(&config_dir(env), session)?;
    tail_lines(&file, HISTORY_BYTES)
}

/// A transcript file's conversation as lines (blocking).
pub fn read(file: &Path) -> Option<Vec<String>> {
    tail_lines(file, HISTORY_BYTES)
}

/// The whole view of a Claude Code session running elsewhere (a terminal, another tool): its
/// timeline, state and changeset, as JSON for the UI. Blocking (reads the transcript and the
/// files it changed).
pub fn view(file: &Path, cwd: &str, started: Option<i64>) -> Option<serde_json::Value> {
    let lines = read(file)?;
    let mut core = Core::new();
    core.state.cwd = cwd.to_owned();
    load_for(&mut core, &lines, started);
    for job in std::mem::take(&mut core.change_jobs) {
        let (change, resolved) = super::diff::file_change(&job);
        core.set_change(&job.abs, change, resolved);
    }
    let events: Vec<&super::model::Event> = core.events().collect();
    Some(json!({"events": events, "state": core.state, "changes": core.changes()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two processes writing to one transcript; a compaction; a tool call with its result.
    const LINES: &[&str] = &[
        r#"{"type":"user","uuid":"u1","parentUuid":null,"session_id":"A","origin":{"kind":"human"},"message":{"role":"user","content":"first prompt"}}"#,
        r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","session_id":"A","message":{"id":"m1","role":"assistant","model":"claude-opus-5-5","content":[{"type":"text","text":"first reply"}]}}"#,
        r#"{"type":"system","subtype":"compact_boundary","uuid":"c1","parentUuid":null,"logicalParentUuid":"a1","compactMetadata":{"preTokens":900000}}"#,
        r#"{"type":"user","uuid":"u2","parentUuid":"c1","session_id":"A","promptSource":"typed","message":{"role":"user","content":"second prompt"}}"#,
        r#"{"type":"user","uuid":"x1","parentUuid":"a1","session_id":"B","origin":{"kind":"human"},"message":{"role":"user","content":"other process"}}"#,
        r#"{"type":"assistant","uuid":"a2","parentUuid":"u2","session_id":"A","message":{"id":"m2","role":"assistant","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/w/a.txt","old_string":"x","new_string":"y"}}]}}"#,
        r#"{"type":"user","uuid":"r1","parentUuid":"a2","session_id":"A","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]},"toolUseResult":{"filePath":"/w/a.txt","originalFile":"x\n","structuredPatch":[{"oldStart":1,"oldLines":1,"newStart":1,"newLines":1,"lines":["-x","+y"]}]}}"#,
        r#"{"type":"assistant","uuid":"a3","parentUuid":"r1","session_id":"A","message":{"id":"m3","role":"assistant","model":"claude-opus-5-5","content":[{"type":"text","text":"edited"}]}}"#,
        r#"{"type":"ai-title","aiTitle":"Fix the thing","sessionId":"S"}"#,
    ];

    #[test]
    fn the_lane_of_a_process_picks_its_branch() {
        // Two processes resumed one session: A started first, B later; each has its own lane.
        let lines: Vec<String> = [
            r#"{"type":"user","uuid":"u0","parentUuid":null,"session_id":"A","timestamp":"2026-10-16T10:00:00Z","origin":{"kind":"human"},"message":{"role":"user","content":"shared start"}}"#,
            r#"{"type":"user","uuid":"a1","parentUuid":"u0","session_id":"A","timestamp":"2026-10-16T10:00:10Z","origin":{"kind":"human"},"message":{"role":"user","content":"from A"}}"#,
            r#"{"type":"user","uuid":"b1","parentUuid":"u0","session_id":"B","timestamp":"2026-10-16T11:00:10Z","origin":{"kind":"human"},"message":{"role":"user","content":"from B"}}"#,
            r#"{"type":"user","uuid":"a2","parentUuid":"a1","session_id":"A","timestamp":"2026-10-16T11:30:00Z","origin":{"kind":"human"},"message":{"role":"user","content":"A again"}}"#,
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let texts = |started: Option<i64>| -> Vec<String> {
            let mut core = Core::new();
            load_for(&mut core, &lines, started);
            core.events().filter_map(|e| match &e.body {
                Body::User { text, .. } => Some(text.clone()),
                _ => None,
            }).collect()
        };
        // B started at 11:00: its lane, though A wrote last.
        let b_start = unix_secs("2026-10-16T11:00:00Z");
        assert_eq!(texts(b_start), vec!["shared start", "from B"]);
        // A started at 09:59.
        assert_eq!(texts(unix_secs("2026-10-16T09:59:58Z")), vec!["shared start", "from A", "A again"]);
        // Unknown start: the newest message's branch.
        assert_eq!(texts(None), vec!["shared start", "from A", "A again"]);
    }

    #[test]
    fn history_follows_the_conversation_branch() {
        let lines: Vec<String> = LINES.iter().map(|s| (*s).to_owned()).collect();
        let mut core = Core::new();
        core.state.cwd = "/w".into();
        load(&mut core, &lines);
        let texts: Vec<String> = core
            .events()
            .filter_map(|e| match &e.body {
                Body::User { text, .. } | Body::Assistant { text, .. } | Body::Notice { text, .. } => Some(text.clone()),
                Body::Tool(t) => Some(format!("tool:{}:{:?}", t.title, t.status)),
                Body::Thinking { .. } => None,
            })
            .collect();
        assert_eq!(texts, vec!["first prompt", "first reply", "Conversation compacted (was 900k tokens)", "second prompt", "tool:a.txt:Done", "edited"]);
        assert!(!texts.iter().any(|t| t == "other process"));
        assert_eq!(core.state.title, "Fix the thing");
        assert_eq!(core.state.goal, "first prompt");
        assert_eq!(core.state.status, super::super::model::Status::Starting);
        assert!(core.take_writes().is_empty());
        assert!(core.change_jobs.iter().any(|j| j.rel == "a.txt"));
    }
}
