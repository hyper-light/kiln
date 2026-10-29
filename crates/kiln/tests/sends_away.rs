//! CP-T3.2 (the twenty-second adversarial check's): a message with an id, sent while its session's
//! runner is away, is refused at once, not parked for that runner. Parked, it went with no epoch to
//! the copy there once the runner was back, though the session had gone on on another runner from
//! the server's store meanwhile (`recover`) and taken the same message there, sent again under its
//! id: one message, two harnesses.
//!
//! `cargo test -p kiln --features chaos --test sends_away -- --nocapture`

#![cfg(all(unix, feature = "chaos"))]

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, http};
use serde_json::{Value, json};

fn connected(port: u16, admin: &str, name: &str) -> anyhow::Result<bool> {
    let (_, body) = http(port, "GET", "/api/agents", admin, None)?;
    let v: Value = serde_json::from_str(&body).unwrap_or_default();
    let list = v.as_array().cloned().or_else(|| v.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
    Ok(list.iter().any(|a| a.get("name").and_then(Value::as_str) == Some(name) && a.get("connected").and_then(Value::as_bool) == Some(true)))
}

/// Every stand-in harness's record of what it was handed, under `dir`.
fn records(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            records(&p, out);
        } else if p.file_name().is_some_and(|n| n == ".fake-claude-messages") {
            out.push(p);
        }
    }
}

/// The pids of the harnesses that were handed message `id`, by every record under `dir`.
fn handed(dir: &Path, id: &str) -> Vec<u64> {
    let mut files = Vec::new();
    records(dir, &mut files);
    let mut pids: Vec<u64> = files
        .iter()
        .flat_map(|f| std::fs::read_to_string(f).unwrap_or_default().lines().map(str::to_owned).collect::<Vec<_>>())
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .filter(|v| v.get("uuid").and_then(Value::as_str) == Some(id))
        .filter_map(|v| v.get("pid").and_then(Value::as_u64))
        .collect();
    pids.sort_unstable();
    pids
}

fn info(port: u16, admin: &str, id: &str) -> anyhow::Result<Value> {
    let (_, body) = http(port, "GET", "/api/harness/sessions", admin, None)?;
    let v: Value = serde_json::from_str(&body).unwrap_or_default();
    let list = v.as_array().cloned().or_else(|| v.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
    Ok(list.into_iter().find(|a| a.get("id").and_then(Value::as_str) == Some(id)).unwrap_or(Value::Null))
}

#[test]
fn a_message_for_a_runner_away_is_refused_and_reaches_one_harness_once_its_session_goes_on_elsewhere() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = common::start_env(dir.path(), &Options { agent: false, ci: "", extra: "" }, false, &[])?;
    // The stand-in keeps Claude Code's own record of each session (what `recover` resumes from),
    // in the runners' home here.
    let claude = dir.path().join("home").join(".claude").to_string_lossy().into_owned();
    let env = [("FAKE_CLAUDE_RECORD", "1"), ("CLAUDE_CONFIG_DIR", claude.as_str())];
    // Runner a's link drops 45 s after each connection, and it stays away 90 s each time.
    let (_a, log_a) = agents::runner_with(dir.path(), s.port, "a", Some("drop=45s,down=90s"), Talk { lines: 1, every_ms: 10 }, &env)?;
    let (_b, log_b) = agents::runner_with(dir.path(), s.port, "b", None, Talk { lines: 1, every_ms: 10 }, &env)?;
    agents::connected(s.port, &s.admin, "a", &log_a, Duration::from_secs(120))?;
    agents::connected(s.port, &s.admin, "b", &log_b, Duration::from_secs(120))?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    std::fs::write(work.join(".fake-claude-messages"), "")?;
    let id = agents::open_session(s.port, &s.admin, "a", &work, Duration::from_secs(90))?;
    // a goes away (its link drops).
    let deadline = Instant::now() + Duration::from_secs(120);
    while connected(s.port, &s.admin, "a")? {
        anyhow::ensure!(Instant::now() < deadline, "a never went away\n{}", s.log());
        std::thread::sleep(Duration::from_millis(200));
    }
    // A message with its own id, sent while a is away: refused at once, so its sender sends it again
    // later, to wherever the session runs then.
    let command = format!("/api/harness/sessions/{id}/command");
    let m = json!({"type": "send", "id": "m-away-once", "text": "deploy the fix, once"});
    let (st1, body1) = http(s.port, "POST", &command, &s.admin, Some(&m))?;
    anyhow::ensure!(st1 == 409, "a message with an id while its runner is away is refused at once: {st1} {body1}\n{}", s.log());
    // The session goes on on b, from the server's store (its machine is away).
    let (st, body) = http(s.port, "POST", "/api/harness/machines/a/recover", &s.admin, Some(&json!({"to": "b"})))?;
    anyhow::ensure!(st == 200 && !body.contains("\"error\""), "recover: {st} {body}\n{}", s.log());
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let i = info(s.port, &s.admin, &id)?;
        let on_b = i.get("target").and_then(Value::as_str) == Some("agent:b");
        let status = i.get("status").and_then(Value::as_str).unwrap_or_default().to_owned();
        if on_b && !status.is_empty() && !matches!(status.as_str(), "starting" | "exited") {
            if let Some(cwd) = i.get("cwd").and_then(Value::as_str) {
                let rec = Path::new(cwd).join(".fake-claude-messages");
                if !rec.exists() {
                    std::fs::write(&rec, "")?;
                }
            }
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the session never ran on b: {i}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(300));
    }
    // Sent again under its id, as its sender does: taken where the session runs now.
    let (st2, body2) = http(s.port, "POST", &command, &s.admin, Some(&m))?;
    anyhow::ensure!((200..300).contains(&st2), "sent again, on b: {st2} {body2}\n{}", s.log());
    std::thread::sleep(Duration::from_secs(3));
    let mut before = handed(dir.path(), "m-away-once");
    before.dedup();
    anyhow::ensure!(before.len() == 1, "on b, one harness had it before a was back: {before:?}");
    anyhow::ensure!(!connected(s.port, &s.admin, "a")?, "a came back too soon for this test: make `down` longer");
    // a comes back: nothing of the message waits for it.
    let deadline = Instant::now() + Duration::from_secs(150);
    while !connected(s.port, &s.admin, "a")? {
        anyhow::ensure!(Instant::now() < deadline, "a never came back\n{}", s.log());
        std::thread::sleep(Duration::from_millis(300));
    }
    std::thread::sleep(Duration::from_secs(15));
    let pids = handed(dir.path(), "m-away-once");
    let mut distinct = pids.clone();
    distinct.dedup();
    anyhow::ensure!(
        distinct == before,
        "one message reached {} harnesses (pids {pids:?}; before runner a was back: {before:?})\nrunner a (last lines): {}",
        distinct.len(),
        std::fs::read_to_string(&log_a).unwrap_or_default().lines().rev().take(30).collect::<Vec<_>>().join("\n")
    );
    Ok(())
}
