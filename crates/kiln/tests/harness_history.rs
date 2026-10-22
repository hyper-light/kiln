//! Native Devin history reaches the same HTTP endpoint Mission Control already uses.

#![cfg(unix)]

mod common;

use std::path::Path;

use common::{Options, http, start};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

const CHILD_DIR: &str = "KILN_DEVIN_HISTORY_TEST_DIR";

fn seed(dir: &Path) -> anyhow::Result<()> {
    let native = dir.join("native/devin/cli");
    std::fs::create_dir_all(&native)?;
    let db = Connection::open(native.join("sessions.db"))?;
    db.execute_batch(
        "CREATE TABLE sessions (id TEXT PRIMARY KEY, working_directory TEXT, model TEXT, title TEXT, created_at INTEGER, last_activity_at INTEGER, main_chain_id INTEGER, hidden INTEGER DEFAULT 0);
         CREATE INDEX idx_sessions_activity ON sessions(last_activity_at DESC);
         CREATE TABLE message_nodes (session_id TEXT, node_id INTEGER, parent_node_id INTEGER, chat_message TEXT, UNIQUE(session_id, node_id));
         INSERT INTO sessions VALUES ('devin-http-fixture', '/synthetic/project', 'synthetic-devin-model', 'Devin HTTP history', 1, 2, 2, 0);",
    )?;
    for (id, parent, role, text) in [(1, None, "user", "Find the synthetic regression"), (2, Some(1), "assistant", "The synthetic check passed")] {
        let message = json!({"role": role, "content": text, "metadata": {"created_at": "2026-10-22T12:00:00Z", "is_user_input": role == "user"}});
        db.execute("INSERT INTO message_nodes VALUES (?1, ?2, ?3, ?4)", params!["devin-http-fixture", id, parent, message.to_string()])?;
    }
    Ok(())
}

fn check_http(dir: &Path) -> anyhow::Result<()> {
    let server = start(dir, &Options::default())?;
    let (status, body) = http(server.port, "GET", "/api/harness/history?target=local&kind=devin&limit=1", &server.admin, None)?;
    assert_eq!(status, 200, "{body}");
    let rows: Value = serde_json::from_str(&body)?;
    assert_eq!(rows, json!([{
        "kind": "devin", "id": "devin-http-fixture", "title": "Devin HTTP history", "cwd": "/synthetic/project",
        "model": "synthetic-devin-model", "last_activity": 2, "last_prompt": "Find the synthetic regression"
    }]));
    let (status, _) = http(server.port, "GET", "/api/harness/history?target=local&kind=unsupported", &server.admin, None)?;
    assert_eq!(status, 400);
    Ok(())
}

#[test]
fn devin_native_history_is_available_through_http() -> anyhow::Result<()> {
    if let Some(dir) = std::env::var_os(CHILD_DIR) {
        return check_http(Path::new(&dir));
    }
    let dir = tempfile::tempdir()?;
    seed(dir.path())?;
    std::fs::create_dir(dir.path().join("bin"))?;
    // Native storage paths belong to this subprocess, without changing concurrent tests' env.
    let output = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "devin_native_history_is_available_through_http", "--nocapture"])
        .env(CHILD_DIR, dir.path())
        .env("PATH", dir.path().join("bin"))
        .env("XDG_DATA_HOME", dir.path().join("native"))
        .env("CLAUDE_CONFIG_DIR", dir.path().join("claude"))
        .env("CODEX_HOME", dir.path().join("codex"))
        .output()?;
    assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    Ok(())
}
