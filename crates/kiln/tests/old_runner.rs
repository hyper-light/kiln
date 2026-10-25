//! Plan step 5 of "Nothing kiln sends between its processes is fire-and-forget" (docs/wire.md): a
//! runner from before the channel still connects and runs agents, as the fleet's runners do while
//! they upgrade after their server. A test build's `old` fault makes the runner say nothing of what
//! it can do past `Build`'s 32 bits, and skip what the server says of its own, as such a runner
//! does: the server sends it commands as it did, and takes its sessions' updates the old way. Its
//! session's turn reaches the server's copy whole, a command and a message reach the harness
//! (answered as sent: the runner can't say more), and a close ends the session.
//!
//! `cargo test -p kiln --features chaos --test old_runner`

#![cfg(all(unix, feature = "chaos"))]

mod common;

use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, http};
use serde_json::json;

/// Waits up to `secs` for `f` to hold; past it, fails with `what` and what `seen` says.
fn until(secs: u64, what: &str, seen: impl Fn() -> String, mut f: impl FnMut() -> anyhow::Result<bool>) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !f()? {
        anyhow::ensure!(Instant::now() < deadline, "{what}\n{}", seen());
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

#[test]
fn a_runner_from_before_the_channel_connects_and_runs_agents() -> anyhow::Result<()> {
    const LINES: usize = 20;
    let dir = tempfile::tempdir()?;
    let s = common::start_env(dir.path(), &Options { agent: false, ci: "", extra: "" }, false, &[])?;
    let (_runner, log) = agents::runner(dir.path(), s.port, "old", Some("old"), Talk { lines: LINES, every_ms: 10 })?;
    agents::connected(s.port, &s.admin, "old", &log, Duration::from_secs(120))?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    let (controls, messages) = (work.join(".fake-claude-controls"), work.join(".fake-claude-messages"));
    std::fs::write(&controls, "")?;
    std::fs::write(&messages, "")?;
    let id = agents::open_session(s.port, &s.admin, "old", &work, Duration::from_secs(90))?;
    let runner_log = || std::fs::read_to_string(&log).unwrap_or_default();
    // What there is to see when something doesn't come: the session as the server has it, what the
    // harness got, and the runner's last words.
    let seen = || {
        let (_, session) = http(s.port, "GET", &format!("/api/harness/sessions/{id}"), &s.admin, None).unwrap_or_default();
        let got = |f: &std::path::Path| std::fs::read_to_string(f).unwrap_or_default();
        let said = runner_log();
        let sends: Vec<&str> = said.lines().filter(|l| l.contains("kiln.sends") || l.contains("inbox")).take(30).collect();
        let tail: Vec<&str> = said.lines().rev().take(30).collect();
        format!(
            "the session: {}\nits controls: {}\nits messages: {}\nthe runner's sends:\n{}\nthe runner's last words:\n{}",
            session.chars().take(3000).collect::<String>(),
            got(&controls),
            got(&messages),
            sends.join("\n"),
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        )
    };

    // Its turn reaches the server's copy, every line of it.
    until(60, "the turn never reached the server's copy whole", seen, || {
        let (frames, _) = agents::attach(s.port, &s.admin, &id, "")?;
        Ok(agents::lines(&agents::events(&frames)).len() == LINES)
    })?;
    // A command reaches the harness, answered as sent.
    let command = format!("/api/harness/sessions/{id}/command");
    let (st, body) = http(s.port, "POST", &command, &s.admin, Some(&json!({"type": "set_model", "model": "model-old"})))?;
    anyhow::ensure!(st == 204, "a command to a runner from before the channel: {st} {body}");
    until(30, "the model never reached the harness", seen, || Ok(std::fs::read_to_string(&controls)?.contains("model-old")))?;
    // So does a message.
    let (st, body) = http(s.port, "POST", &command, &s.admin, Some(&json!({"type": "send", "id": "m-old-1", "text": "hello from before"})))?;
    anyhow::ensure!((200..300).contains(&st), "a message to a runner from before the channel: {st} {body}");
    until(30, "the message never reached the harness", seen, || Ok(std::fs::read_to_string(&messages)?.contains("hello from before")))?;
    // And a close ends the session.
    let (st, body) = http(s.port, "DELETE", &format!("/api/harness/sessions/{id}"), &s.admin, None)?;
    anyhow::ensure!((200..300).contains(&st), "a close: {st} {body}");
    until(30, "the session never closed", seen, || Ok(http(s.port, "GET", &format!("/api/harness/sessions/{id}"), &s.admin, None)?.0 == 404))?;
    // All of it the old way: none of its session's updates went on the channel.
    let said = runner_log();
    anyhow::ensure!(!said.contains("updates start on the channel"), "a runner from before the channel sent its updates on it:\n{said}");
    Ok(())
}
