#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, Server, http, start, start_env};
use serde_json::{Value, json};

fn login_shell(dir: &Path, path: &str) -> anyhow::Result<String> {
    let shell = dir.join("login-shell");
    std::fs::write(&shell, format!("#!/bin/sh\nPATH='{path}'\nexport PATH\nwhile [ $# -gt 1 ]; do shift; done\nexec /bin/sh -c \"$1\"\n"))?;
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755))?;
    Ok(shell.to_string_lossy().into_owned())
}

fn session(s: &Server, id: &str) -> anyhow::Result<Value> {
    Ok(serde_json::from_str(&http(s.port, "GET", &format!("/api/harness/sessions/{id}"), &s.admin, None)?.1).unwrap_or_default())
}

fn open(s: &Server, target: &str, work: &Path) -> anyhow::Result<String> {
    let open = json!({"target": target, "spec": {"kind": "claude", "cwd": work.display().to_string(), "prompt": "stream"}});
    let (code, body) = http(s.port, "POST", "/api/harness/sessions", &s.admin, Some(&open))?;
    anyhow::ensure!(code == 200, "couldn't open it on {target}: {code} {body}");
    Ok(serde_json::from_str::<Value>(&body)?.pointer("/0/id").and_then(Value::as_str).unwrap_or_default().to_owned())
}

fn stopped(s: &Server, id: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let v = session(s, id)?;
        let error = v.pointer("/exit/error").and_then(Value::as_str).unwrap_or_default();
        if v.get("status").and_then(Value::as_str) == Some("exited") && !error.is_empty() {
            anyhow::ensure!(error.contains("is not installed on this machine"), "it stopped for another reason: {v}");
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "it never stood stopped: {v}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn answered(s: &Server, id: &str, target: &str, logs: &[&Path]) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let (frames, _) = agents::attach(s.port, &s.admin, id, "")?;
        let lines = agents::lines(&agents::events(&frames));
        let v = session(s, id)?;
        if lines.len() >= 3 && v.get("target").and_then(Value::as_str) == Some(target) {
            return Ok(());
        }
        if Instant::now() > deadline {
            let said: Vec<String> = logs.iter().map(|l| std::fs::read_to_string(l).unwrap_or_default()).collect();
            anyhow::bail!("its first message was never answered on {target} (lines {lines:?}): {v}\n{}\n{}", s.log(), said.join("\n"));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn offers_claude(s: &Server, runner: &str, log: &Path) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = http(s.port, "GET", &format!("/api/harness/kinds?target=agent:{runner}"), &s.admin, None)?;
        let kinds: Value = serde_json::from_str(&body).unwrap_or_default();
        let claude = kinds.as_array().into_iter().flatten().find(|k| k.get("id").and_then(Value::as_str) == Some("claude"));
        if claude.and_then(|k| k.get("available")).and_then(Value::as_bool) == Some(true) {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "{runner} never offered Claude Code: {body}\n{}", std::fs::read_to_string(log).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn install_claude(bin: &Path) -> anyhow::Result<()> {
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-claude/claude");
    std::fs::copy(fake, bin.join("claude"))?;
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[test]
fn an_agent_that_couldnt_start_on_the_server_moves_to_a_runner_that_has_its_harness() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let shell = login_shell(dir.path(), "/usr/bin:/bin")?;
    let s = start_env(dir.path(), &Options { extra: "[terminal]\nenabled = true\n", ..Options::default() }, false, &[("PATH", "/usr/bin:/bin"), ("SHELL", &shell)])?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;

    let id = open(&s, "local", &work)?;
    stopped(&s, &id)?;

    let (_runner, log) = agents::runner(dir.path(), s.port, "box", None, Talk { lines: 3, every_ms: 20 })?;
    agents::connected(s.port, &s.admin, "box", &log, Duration::from_secs(120))?;
    offers_claude(&s, "box", &log)?;

    let (code, body) = http(s.port, "POST", "/api/harness/agents", &s.admin, Some(&json!({"kind": "claude", "on": "local", "cwd": work.display().to_string()})))?;
    anyhow::ensure!(code == 409 && body.contains("Claude Code isn't installed on the kiln server") && body.contains("with `on`: box"), "{code} {body}");

    let (code, body) = http(s.port, "POST", &format!("/api/harness/sessions/{id}/move"), &s.admin, Some(&json!({"to": "agent:box"})))?;
    anyhow::ensure!(code == 202, "the move was refused: {code} {body}");
    let moved: Value = serde_json::from_str(&body)?;
    anyhow::ensure!(moved.get("moving").and_then(Value::as_str) == Some("box") && moved.get("from_server") == Some(&json!(true)), "{moved}");
    answered(&s, &id, "agent:box", &[&log])?;

    let (code, body) = http(s.port, "POST", &format!("/api/harness/sessions/{id}/move"), &s.admin, Some(&json!({"to": "agent:box"})))?;
    anyhow::ensure!(code == 409 && body.contains("it runs on box already"), "{code} {body}");
    Ok(())
}

#[test]
fn a_runner_agent_that_couldnt_start_keeps_its_first_message_moved_or_started_again() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    let bare_bin = dir.path().join("bare-bin");
    std::fs::create_dir_all(&bare_bin)?;
    let path = format!("{}:/usr/bin:/bin", bare_bin.display());
    let shell = login_shell(dir.path(), &path)?;
    let bare_env = [("PATH", path.as_str()), ("SHELL", shell.as_str())];
    let talk = Talk { lines: 3, every_ms: 20 };
    let (bare, bare_log) = agents::runner_env(dir.path(), s.port, "bare", None, talk, &bare_env)?;
    let (_box, box_log) = agents::runner(dir.path(), s.port, "box", None, talk)?;
    agents::connected(s.port, &s.admin, "bare", &bare_log, Duration::from_secs(120))?;
    agents::connected(s.port, &s.admin, "box", &box_log, Duration::from_secs(120))?;
    offers_claude(&s, "box", &box_log)?;
    let logs = [bare_log.as_path(), box_log.as_path()];

    let moved = open(&s, "agent:bare", &work)?;
    stopped(&s, &moved)?;
    let (code, body) = http(s.port, "POST", &format!("/api/harness/sessions/{moved}/move"), &s.admin, Some(&json!({"to": "agent:box"})))?;
    anyhow::ensure!(code == 202, "the move was refused: {code} {body}");
    answered(&s, &moved, "agent:box", &logs)?;

    let kept = open(&s, "agent:bare", &work)?;
    stopped(&s, &kept)?;
    std::thread::sleep(Duration::from_secs(3));
    drop(bare);
    install_claude(&bare_bin)?;
    let (_bare, bare_log) = agents::runner_env(dir.path(), s.port, "bare", None, talk, &bare_env)?;
    agents::connected(s.port, &s.admin, "bare", &bare_log, Duration::from_secs(120))?;
    answered(&s, &kept, "agent:bare", &logs)?;

    std::fs::remove_file(bare_bin.join("claude"))?;
    let restarted = open(&s, "agent:bare", &work)?;
    stopped(&s, &restarted)?;
    install_claude(&bare_bin)?;
    let (code, body) = http(s.port, "POST", &format!("/api/harness/sessions/{restarted}/command"), &s.admin, Some(&json!({"type": "restart"})))?;
    anyhow::ensure!((200..300).contains(&code), "the restart was refused: {code} {body}");
    answered(&s, &restarted, "agent:bare", &logs)?;
    Ok(())
}
