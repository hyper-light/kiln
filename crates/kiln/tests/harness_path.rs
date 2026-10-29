//! The harnesses a runner offers are the ones its user has. A runner's service keeps the PATH of
//! the terminal it was installed from, so a harness installed since in a folder that PATH lacks
//! (Claude Code's and Codex's installers put theirs in `~/.local/bin`) is on the user's PATH alone,
//! as their login shell has it: the runner offers it all the same, and an agent started there runs
//! it (docs/agents.md, "Where a harness is found").
//!
//! `cargo test -p kiln --test harness_path`

#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, http, start};
use serde_json::Value;

#[test]
fn a_harness_on_its_users_path_alone_is_offered_and_runs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    // The user's login shell has the folder the stand-in Claude Code is in on its PATH; the
    // runner's own PATH, a service's, hasn't it. Neither has a harness installed system-wide.
    let system = agents::path_without_harnesses(dir.path())?;
    let shell = dir.path().join("login-shell");
    let script = format!("#!/bin/sh\nPATH='{}:{system}'\nexport PATH\nwhile [ $# -gt 1 ]; do shift; done\nexec /bin/sh -c \"$1\"\n", dir.path().join("bin").display());
    std::fs::write(&shell, script)?;
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755))?;
    let shell = shell.to_string_lossy().into_owned();
    let (_runner, log) = agents::runner_env(dir.path(), s.port, "box", None, Talk { lines: 3, every_ms: 20 }, &[("PATH", &system), ("SHELL", &shell)])?;
    agents::connected(s.port, &s.admin, "box", &log, Duration::from_secs(120))?;
    let said = || std::fs::read_to_string(&log).unwrap_or_default();

    // It offers Claude Code.
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

    // An agent started there runs it: its turn comes.
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    let id = agents::open_session(s.port, &s.admin, "box", &work, Duration::from_secs(60))?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (frames, _) = agents::attach(s.port, &s.admin, &id, "")?;
        let lines = agents::lines(&agents::events(&frames));
        if lines.len() >= 3 {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the agent's turn never came (lines {lines:?}): {}\n{}", serde_json::to_string(&frames)?, said());
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}
