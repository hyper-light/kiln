//! Plan step 3 of "Nothing kiln sends between its processes is fire-and-forget" (docs/wire.md,
//! "Commands"): an agent session's commands ride its runner's command streams, and each caller gets
//! the runner's answer, never a "sent" for a frame queued. The runner's link is cut for 2 s every
//! 6 s; one in three of the server's channel frames to it never reaches the wire, and one in three
//! of the runner's (its answers among them), so the server sends again what the runner took
//! already, and the runner gets it twice. Through that:
//!
//! - messages, which wait for their runner, are each applied once: one under way as the link was
//!   cut is answered taken once the runner is back, and one sent while it's away is answered parked
//!   at once and goes once it's back;
//! - model changes, refused while the runner is away, are applied once at most, and each one
//!   answered taken is applied;
//! - a close while the runner is away is answered at once and goes once it's back. A message sent
//!   to the session after it goes after the close too: the runner, which hasn't the session any
//!   more, refuses it (`unknown`), and it's never applied. A command to the session then is
//!   answered 404, never taken.
//!
//! The harness is a stand-in for Claude Code (`tests/fixtures/fake-claude`), which writes each
//! control request it gets (a model to change to, here) and each message to files the test reads.
//!
//! `cargo test -p kiln --features chaos --test harness_commands`

#![cfg(all(unix, feature = "chaos"))]

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, http};
use serde_json::{Value, json};

/// Whether runner `name` is connected now.
fn connected(port: u16, admin: &str, name: &str) -> anyhow::Result<bool> {
    let (_, body) = http(port, "GET", "/api/agents", admin, None)?;
    let v: Value = serde_json::from_str(&body).unwrap_or_default();
    let list = v.as_array().cloned().or_else(|| v.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
    Ok(list.iter().any(|a| a.get("name").and_then(Value::as_str) == Some(name) && a.get("connected").and_then(Value::as_bool) == Some(true)))
}

/// How many times the runner has cut its link so far.
fn cuts(log: &Path) -> usize {
    std::fs::read_to_string(log).map(|l| l.matches("chaos: dropping the connection").count()).unwrap_or(0)
}

/// How many of the lines of `file` (one per thing the harness got) each of `names` is in.
fn times(file: &Path, names: &[String]) -> anyhow::Result<BTreeMap<String, usize>> {
    let text = std::fs::read_to_string(file)?;
    Ok(names.iter().map(|n| (n.clone(), text.lines().filter(|l| l.contains(n.as_str())).count())).filter(|(_, c)| *c > 0).collect())
}

#[test]
fn a_sessions_commands_are_answered_by_its_runner_through_cuts_and_lost_frames() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // About one in three of the server's channel frames to its runners (`ServerMsg::Wire`, variant
    // 54) never reach the wire.
    let s = common::start_env(dir.path(), &Options { agent: false, ci: "", extra: "" }, false, &[("KILN_CHAOS", "shed=control/54:3")])?;
    // Its link cut for 2 s every 6 s, and about one in three of its own channel frames
    // (`AgentMsg::Wire`, variant 46, on the terminal lane) lost.
    let (_runner, log) = agents::runner(dir.path(), s.port, "box", Some("drop=6s,down=2s,shed=term/46:3"), Talk { lines: 1, every_ms: 10 })?;
    agents::connected(s.port, &s.admin, "box", &log, Duration::from_secs(120))?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    let (controls, messages) = (work.join(".fake-claude-controls"), work.join(".fake-claude-messages"));
    std::fs::write(&controls, "")?;
    std::fs::write(&messages, "")?;
    let id = agents::open_session(s.port, &s.admin, "box", &work, Duration::from_secs(90))?;
    let command = format!("/api/harness/sessions/{id}/command");
    // A message without an id rides the session's runner's parked command stream (with one, it
    // goes through the session's inbox, which answers it: CP-T3.2): a skill to run, here, which
    // the harness gets as `/kiln-test <text>`.
    let message = |text: &str| json!({"type": "skill_run", "name": "kiln-test", "args": text});

    // Messages, one after another, until two cuts have fallen among them, and one on a message
    // under way. One under way as the link is cut waits for the runner to come back, then goes on.
    let (mut sent, mut taken, mut parked, mut across) = (Vec::new(), 0usize, 0usize, 0usize);
    let deadline = Instant::now() + Duration::from_secs(180);
    let first = cuts(&log);
    while sent.len() < 20 || cuts(&log) < first + 2 || across == 0 {
        anyhow::ensure!(Instant::now() < deadline, "{} messages: {taken} taken ({across} across a cut), through {} cuts\n{}", sent.len(), cuts(&log) - first, s.log());
        let text = format!("message-{:03}", sent.len());
        let (before, t) = (cuts(&log), Instant::now());
        let (st, body) = http(s.port, "POST", &command, &s.admin, Some(&message(&text)))?;
        anyhow::ensure!(st == 204, "{text}: {st} {body}\n{}", s.log());
        taken += 1;
        // Across a cut: it waited for the runner to come back (2 s away).
        across += usize::from(cuts(&log) > before && t.elapsed() > Duration::from_millis(1500));
        sent.push(text);
    }
    // Then a few while the runner is away: answered parked at once, they go once it's back.
    let deadline = Instant::now() + Duration::from_secs(60);
    while parked < 3 {
        anyhow::ensure!(Instant::now() < deadline, "{parked} messages parked\n{}", s.log());
        if connected(s.port, &s.admin, "box")? {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        let text = format!("message-{:03}", sent.len());
        let (st, body) = http(s.port, "POST", &command, &s.admin, Some(&message(&text)))?;
        match st {
            202 => parked += 1,
            // Back already.
            204 => taken += 1,
            _ => anyhow::bail!("{text}: {st} {body}\n{}", s.log()),
        }
        sent.push(text);
    }
    // What the harness got: each message once.
    let deadline = Instant::now() + Duration::from_secs(30);
    let got = loop {
        let got = times(&messages, &sent)?;
        if got.len() == sent.len() || Instant::now() > deadline {
            break got;
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    let twice: Vec<&String> = got.iter().filter(|(_, n)| **n > 1).map(|(m, _)| m).collect();
    assert!(twice.is_empty(), "messages applied more than once: {twice:?}");
    let missed: Vec<&String> = sent.iter().filter(|m| !got.contains_key(*m)).collect();
    if !missed.is_empty() {
        let (_, session) = http(s.port, "GET", &format!("/api/harness/sessions/{id}"), &s.admin, None)?;
        let said = std::fs::read_to_string(&messages)?;
        anyhow::bail!("messages answered taken or parked, never applied: {missed:?}\nthe harness got: {}\nthe session: {}", said.chars().take(1500).collect::<String>(), session.chars().take(3000).collect::<String>());
    }
    eprintln!("{} messages: {taken} taken ({across} across a cut), {parked} parked while away, each applied once", sent.len());

    // Each model once, asked again while its runner is away (refused at once, never kept for
    // later), until 30 and two cuts among them.
    let (mut asked, mut models, mut unanswered, mut away) = (Vec::new(), Vec::new(), 0usize, 0usize);
    let first = cuts(&log);
    let deadline = Instant::now() + Duration::from_secs(180);
    while asked.len() < 30 || cuts(&log) < first + 2 {
        let model = format!("model-{:03}", asked.len());
        asked.push(model.clone());
        loop {
            anyhow::ensure!(Instant::now() < deadline, "{model} never taken: {} models through {} cuts\n{}", asked.len(), cuts(&log) - first, s.log());
            let (st, body) = http(s.port, "POST", &command, &s.admin, Some(&json!({"type": "set_model", "model": model})))?;
            match st {
                204 => {
                    models.push(model.clone());
                    break;
                }
                // Its link dropped before it answered: it may or may not have changed.
                504 => {
                    unanswered += 1;
                    break;
                }
                409 => {
                    away += 1;
                    std::thread::sleep(Duration::from_millis(300));
                }
                _ => anyhow::bail!("{model}: {st} {body}\n{}", s.log()),
            }
        }
    }
    // What the harness got: each model at most once, and each taken one exactly once.
    let deadline = Instant::now() + Duration::from_secs(30);
    let got = loop {
        let got = times(&controls, &asked)?;
        if models.iter().all(|m| got.contains_key(m)) || Instant::now() > deadline {
            break got;
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    let twice: Vec<&String> = got.iter().filter(|(_, n)| **n > 1).map(|(m, _)| m).collect();
    assert!(twice.is_empty(), "models applied more than once: {twice:?}");
    let missed: Vec<&String> = models.iter().filter(|m| !got.contains_key(*m)).collect();
    assert!(missed.is_empty(), "models answered taken, never applied: {missed:?}");
    eprintln!("{} models: {} taken, {unanswered} unanswered (applied: {}), asked again {away} times while away, across {} cuts", asked.len(), models.len(), got.len(), cuts(&log) - first);

    // A close while its runner is away: answered at once (it waits for the runner), and so is a
    // message sent after it, which waits behind it.
    let deadline = Instant::now() + Duration::from_secs(30);
    while connected(s.port, &s.admin, "box")? {
        anyhow::ensure!(Instant::now() < deadline, "the link never went down");
        std::thread::sleep(Duration::from_millis(100));
    }
    let t = Instant::now();
    let (st, body) = http(s.port, "DELETE", &format!("/api/harness/sessions/{id}"), &s.admin, None)?;
    assert!((200..300).contains(&st) && t.elapsed() < Duration::from_secs(5), "a close while its runner is away waits for it, not its caller: {st} {body}");
    let (st, body) = http(s.port, "POST", &command, &s.admin, Some(&message("after-close")))?;
    assert_eq!(st, 202, "a message while its runner is away is parked: {body}");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (st, _) = http(s.port, "GET", &format!("/api/harness/sessions/{id}"), &s.admin, None)?;
        if st == 404 {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the session never closed once its runner was back\n{}", s.log());
        std::thread::sleep(Duration::from_millis(300));
    }
    // The message went after the close: the runner refused it, not having the session, and the
    // server says so (it was answered parked: nobody waits for it).
    let refused = format!("no session {id} here");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !std::fs::read_to_string(dir.path().join("server.log"))?.contains(&refused) {
        anyhow::ensure!(Instant::now() < deadline, "the runner never refused the message after the close\n{}", s.log());
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(times(&messages, &["after-close".to_owned()])?.is_empty(), "a message after its session's close was applied");
    // Then a command to it is answered, and not as taken.
    let (st, body) = http(s.port, "POST", &command, &s.admin, Some(&json!({"type": "set_model", "model": "after"})))?;
    assert_eq!(st, 404, "a command to a closed session: {body}");
    Ok(())
}
