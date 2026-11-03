//! CP-T3.2, as the "Tabs UI" agent met it (2026-10-31): a message sent to a Codex agent as Codex
//! started waits in its adapter for the thread, shown queued by the agent, and a switch to Claude
//! Code (at once: nothing is under way) ended Codex's process with that message still in it: never
//! sent, it said queued for good, and Claude Code never heard it. Through a server and a runner,
//! with a Codex that never opens a thread and Claude Code's stand-in (`tests/fixtures/fake-claude`):
//! the message reaches Claude Code once it's up, first, and ends done; one sent after the switch
//! goes after it.
//!
//! `cargo test -p kiln --test switch_held`

#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, Server, http, start};
use serde_json::{Value, json};

/// The session's state as a viewer attaching to it now gets it.
fn state(s: &Server, id: &str) -> anyhow::Result<Value> {
    let (frames, _) = agents::attach(s.port, &s.admin, id, "")?;
    Ok(frames.iter().rev().find(|f| f.get("t").and_then(Value::as_str) == Some("state")).and_then(|f| f.get("state")).cloned().unwrap_or_default())
}

/// The ids of the messages Claude Code's stand-in got, in order.
fn heard(file: &Path) -> Vec<String> {
    std::fs::read_to_string(file).unwrap_or_default().lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter_map(|v| v.get("uuid").and_then(Value::as_str).map(str::to_owned)).collect()
}

#[test]
fn a_message_held_for_codex_goes_to_claude_code_when_the_agent_switches() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    // A Codex that never opens a thread (it never answers), found first on the runner's PATH, as
    // Claude Code's stand-in is.
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin)?;
    std::fs::write(bin.join("codex"), "#!/bin/sh\nexec cat >/dev/null\n")?;
    std::fs::set_permissions(bin.join("codex"), std::fs::Permissions::from_mode(0o755))?;
    let (_runner, log) = agents::runner(dir.path(), s.port, "box", None, Talk { lines: 3, every_ms: 20 })?;
    agents::connected(s.port, &s.admin, "box", &log, Duration::from_secs(120))?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    let said = work.join(".fake-claude-messages");
    std::fs::write(&said, "")?;
    let open = json!({"target": format!("agent:{}", common::machine("box")), "spec": {"kind": "codex", "cwd": work.display().to_string()}});
    let (code, body) = http(s.port, "POST", "/api/harness/sessions", &s.admin, Some(&open))?;
    anyhow::ensure!(code == 200, "couldn't open it: {code} {body}");
    let id = serde_json::from_str::<Value>(&body)?.pointer("/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let command = format!("/api/harness/sessions/{id}/command");
    let send = |mid: &str, text: &str| -> anyhow::Result<()> {
        let (code, body) = http(s.port, "POST", &command, &s.admin, Some(&json!({"type": "send", "id": mid, "text": text, "mode": "queue"})))?;
        anyhow::ensure!((200..300).contains(&code), "{mid}: {code} {body}");
        Ok(())
    };
    let until = |what: &str, within: Duration, done: &dyn Fn(&Value) -> bool| -> anyhow::Result<Value> {
        let deadline = Instant::now() + within;
        loop {
            let st = state(&s, &id)?;
            if done(&st) {
                return Ok(st);
            }
            anyhow::ensure!(Instant::now() < deadline, "{what} never came: {st}\nClaude Code's stand-in heard {:?}\n{}", heard(&said), s.log());
            std::thread::sleep(Duration::from_millis(250));
        }
    };
    let delivery = |st: &Value, mid: &str| st.pointer(&format!("/delivery/{mid}")).and_then(Value::as_str).map(str::to_owned);

    let first = "01a19347-2437-7490-b35b-000000000001";
    send(first, "Plans and Research, one tab each")?;
    until("the message held for Codex's thread", Duration::from_secs(60), &|st| delivery(st, first).as_deref() == Some("queued"))?;
    let (code, body) = http(s.port, "POST", &command, &s.admin, Some(&json!({"type": "set_model", "model": "", "kind": "claude"})))?;
    anyhow::ensure!((200..300).contains(&code), "the switch: {code} {body}");
    // Sent as Claude Code starts: it goes after the one held for Codex.
    let then = "01a19347-2437-7490-b35b-000000000002";
    send(then, "continue")?;
    until("both done in Claude Code", Duration::from_secs(90), &|st| delivery(st, first).as_deref() == Some("completed") && delivery(st, then).as_deref() == Some("completed"))?;
    assert_eq!(heard(&said), [first, then], "Claude Code heard each once, in the order they were sent");
    Ok(())
}
