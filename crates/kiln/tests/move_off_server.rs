#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, http, start_env};
use serde_json::{Value, json};

#[test]
fn an_agent_that_couldnt_start_on_the_server_moves_to_a_runner_that_has_its_harness() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let shell = dir.path().join("login-shell");
    std::fs::write(&shell, "#!/bin/sh\nPATH='/usr/bin:/bin'\nexport PATH\nwhile [ $# -gt 1 ]; do shift; done\nexec /bin/sh -c \"$1\"\n")?;
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755))?;
    let shell = shell.to_string_lossy().into_owned();
    let s = start_env(dir.path(), &Options { extra: "[terminal]\nenabled = true\n", ..Options::default() }, false, &[("PATH", "/usr/bin:/bin"), ("SHELL", &shell)])?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    let session = |id: &str| -> anyhow::Result<Value> { Ok(serde_json::from_str(&http(s.port, "GET", &format!("/api/harness/sessions/{id}"), &s.admin, None)?.1).unwrap_or_default()) };

    let open = json!({"target": "local", "spec": {"kind": "claude", "cwd": work.display().to_string(), "prompt": "stream"}});
    let (code, body) = http(s.port, "POST", "/api/harness/sessions", &s.admin, Some(&open))?;
    anyhow::ensure!(code == 200, "couldn't open it: {code} {body}");
    let id = serde_json::from_str::<Value>(&body)?.pointer("/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let v = session(&id)?;
        let error = v.pointer("/exit/error").and_then(Value::as_str).unwrap_or_default();
        if v.get("status").and_then(Value::as_str) == Some("exited") && !error.is_empty() {
            anyhow::ensure!(error.contains("is not installed on this machine"), "it stopped for another reason: {v}");
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "it never stood stopped: {v}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(200));
    }

    let (_runner, log) = agents::runner(dir.path(), s.port, "box", None, Talk { lines: 3, every_ms: 20 })?;
    agents::connected(s.port, &s.admin, "box", &log, Duration::from_secs(120))?;
    let said = || std::fs::read_to_string(&log).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = http(s.port, "GET", "/api/harness/kinds?target=agent:box", &s.admin, None)?;
        let kinds: Value = serde_json::from_str(&body).unwrap_or_default();
        let claude = kinds.as_array().into_iter().flatten().find(|k| k.get("id").and_then(Value::as_str) == Some("claude"));
        if claude.and_then(|k| k.get("available")).and_then(Value::as_bool) == Some(true) {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "box never offered Claude Code: {body}\n{}", said());
        std::thread::sleep(Duration::from_millis(300));
    }

    let (code, body) = http(s.port, "POST", "/api/harness/agents", &s.admin, Some(&json!({"kind": "claude", "on": "local", "cwd": work.display().to_string()})))?;
    anyhow::ensure!(code == 409 && body.contains("Claude Code isn't installed on the kiln server") && body.contains("with `on`: box"), "{code} {body}");

    let (code, body) = http(s.port, "POST", &format!("/api/harness/sessions/{id}/move"), &s.admin, Some(&json!({"to": "agent:box"})))?;
    anyhow::ensure!(code == 202, "the move was refused: {code} {body}");
    let moved: Value = serde_json::from_str(&body)?;
    anyhow::ensure!(moved.get("moving").and_then(Value::as_str) == Some("box") && moved.get("from_server") == Some(&json!(true)), "{moved}");
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let (frames, _) = agents::attach(s.port, &s.admin, &id, "")?;
        let lines = agents::lines(&agents::events(&frames));
        let v = session(&id)?;
        if lines.len() >= 3 && v.get("target").and_then(Value::as_str) == Some("agent:box") {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "it never went on on box (lines {lines:?}): {v}\n{}\n{}", s.log(), said());
        std::thread::sleep(Duration::from_millis(500));
    }

    let (code, body) = http(s.port, "POST", &format!("/api/harness/sessions/{id}/move"), &s.admin, Some(&json!({"to": "agent:box"})))?;
    anyhow::ensure!(code == 409 && body.contains("it runs on box already"), "{code} {body}");
    Ok(())
}
