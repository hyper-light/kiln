//! How many agent sessions a machine has at work at once (docs/agents.md, "How many agents a
//! machine runs"), end to end: a server, a runner and the stand-in Claude Code it runs
//! (`tests/fixtures/fake-claude`), whose first turns go on until a file says go. Only agents at work
//! count. The runner's config says 1: while one works a second is refused, saying so, and once that
//! one has finished another starts. Strictly: the finished one, sent a message while the other
//! works, waits for room, and goes to work once the other's turn is over. Set to 3 on the server,
//! which wins, it takes two more and says it holds 3, set there, its config's 1 beside it. Taken
//! away on the server, its config's 1 holds again, and those at work go on. Its config changed, the
//! new one holds within seconds, nothing restarting. Started again, it holds what the server set
//! before it's connected, and every session it ran comes back.
//!
//! `cargo test -p kiln --test agent_limit`

#![cfg(unix)]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, http, http_within};
use serde_json::{Value, json};

/// A turn after a session's first (none here) goes on for as long as the test does.
const TALK: Talk = Talk { lines: 100_000, every_ms: 500 };

/// The runner's agent limit, as the server says it.
fn view(port: u16, admin: &str, name: &str) -> anyhow::Result<Value> {
    let (st, body) = http(port, "GET", &format!("/api/runners/{name}/agent-limit"), admin, None)?;
    anyhow::ensure!(st == 200, "GET its agent limit: {st} {body}");
    Ok(serde_json::from_str(&body)?)
}

/// Waits until the server's view of runner `name`'s agent limit is as `holds` says.
fn until(port: u16, admin: &str, name: &str, what: &str, log: &Path, holds: impl Fn(&Value) -> bool) -> anyhow::Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let v = view(port, admin, name)?;
        if holds(&v) {
            return Ok(v);
        }
        anyhow::ensure!(Instant::now() < deadline, "{name} never {what}: {v}\n{}", tail(log));
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Waits until the runner has said it holds at most `max`, from `from`.
fn holds(port: u16, admin: &str, name: &str, (max, from): (u64, &str), log: &Path) -> anyhow::Result<Value> {
    until(port, admin, name, &format!("said it holds {max} from {from}"), log, |v| {
        (v.pointer("/held/max_sessions").and_then(Value::as_u64), v.pointer("/held/from").and_then(Value::as_str)) == (Some(max), Some(from))
    })
}

/// Waits until `n` of the runner's agents are at work.
fn working(port: u16, admin: &str, name: &str, n: u64, log: &Path) -> anyhow::Result<()> {
    until(port, admin, name, &format!("had {n} at work"), log, |v| v.get("working").and_then(Value::as_u64) == Some(n)).map(drop)
}

/// One try at a session on runner `name`: the status and the body.
fn open(port: u16, admin: &str, name: &str, cwd: &Path) -> anyhow::Result<(u16, String)> {
    let body = json!({"target": format!("agent:{name}"), "spec": {"kind": "claude", "cwd": cwd.display().to_string(), "prompt": "stream"}});
    // Waited for longer than the server waits for its runner's answer, so it's the server's answer.
    http_within(port, "POST", "/api/harness/sessions", admin, Some(&body), Duration::from_secs(75))
}

/// A session on runner `name`, which takes it: its id.
fn opened(port: u16, admin: &str, name: &str, cwd: &Path) -> anyhow::Result<String> {
    let (st, body) = open(port, admin, name, cwd)?;
    anyhow::ensure!(st == 200, "a session wasn't taken: {st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    v.pointer("/0/id").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no id: {body}"))
}

/// What a machine that's full says: how many are at work (and wait for room), and its limit.
fn full(n: u64, waiting: u64, max: u64) -> String {
    let waiting = if waiting == 0 { String::new() } else { format!(" and {waiting} waiting for room") };
    format!("already has {n} agent{} at work{waiting} (its limit is {max})", if n == 1 { "" } else { "s" })
}

/// A session refused, the machine saying it's `full`.
fn refused(port: u16, admin: &str, name: &str, cwd: &Path, full: &str) -> anyhow::Result<()> {
    let (st, body) = open(port, admin, name, cwd)?;
    anyhow::ensure!(st == 400 && body.contains(full) && body.contains("[agent] max_agent_sessions"), "not refused for {full}: {st} {body}");
    Ok(())
}

fn put(port: u16, admin: &str, name: &str, body: &Value) -> anyhow::Result<(u16, Value)> {
    let (st, text) = http(port, "PUT", &format!("/api/runners/{name}/agent-limit"), admin, Some(body))?;
    Ok((st, serde_json::from_str(&text).unwrap_or(Value::String(text))))
}

/// A command to session `id`, as a person gives it from its page: taken.
fn command(port: u16, admin: &str, id: &str, c: &Value) -> anyhow::Result<()> {
    let (st, said) = http(port, "POST", &format!("/api/harness/sessions/{id}/command"), admin, Some(c))?;
    anyhow::ensure!((200..300).contains(&st), "{c} to {id}: {st} {said}");
    Ok(())
}

/// Waits until session `id`, as the server has it, is as `holds` says.
fn session_until(port: u16, admin: &str, id: &str, what: &str, log: &Path, holds: impl Fn(&Value) -> bool) -> anyhow::Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = http(port, "GET", &format!("/api/harness/sessions/{id}"), admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        if holds(&v) {
            return Ok(v);
        }
        anyhow::ensure!(Instant::now() < deadline, "{id} never {what}: {v}\n{}", tail(log));
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// The end of a runner's log, to say why a test failed.
fn tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines.iter().rev().take(40).rev().copied().collect::<Vec<_>>().join("\n")
}

#[test]
fn only_agents_at_work_count_toward_a_runners_limit_set_on_the_server_over_its_config() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = common::start(dir.path(), &Options::default())?;
    let config = dir.path().join("box.toml");
    std::fs::write(&config, "[agent]\nmax_agent_sessions = 1\n")?;
    let config_path = config.display().to_string();
    // A new session's first turn: once the file `go` names is there, one line, and it's finished.
    let go = dir.path().join("go");
    let go_path = go.display().to_string();
    let env = [("KILN_CONFIG", config_path.as_str()), ("FAKE_CLAUDE_GO", go_path.as_str()), ("FAKE_CLAUDE_FIRST_LINES", "1")];
    let (runner, log) = agents::runner_with(dir.path(), s.port, "box", None, TALK, &env)?;
    agents::connected(s.port, &s.admin, "box", &log, Duration::from_secs(120))?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    let kept = dir.path().join("box-state/agent-limit.json");

    // Its config's 1: one at work, and a second refused, saying how many, its limit, and the way up.
    let v = holds(s.port, &s.admin, "box", (1, "config"), &log)?;
    assert_eq!((v.get("max_sessions"), v.get("supported"), v.get("runs_agents")), (Some(&Value::Null), Some(&json!(true)), Some(&json!(true))), "{v}");
    let first = agents::open_session(s.port, &s.admin, "box", &work, Duration::from_secs(90))?;
    let mut ids = vec![first.clone()];
    working(s.port, &s.admin, "box", 1, &log)?;
    refused(s.port, &s.admin, "box", &work, &full(1, 0, 1))?;

    // Its turn over, it's finished: it no longer counts, and another starts. That one at work, a
    // third is refused.
    std::fs::write(&go, "")?;
    working(s.port, &s.admin, "box", 0, &log)?;
    std::fs::remove_file(&go)?;
    ids.push(opened(s.port, &s.admin, "box", &work)?);
    working(s.port, &s.admin, "box", 1, &log)?;
    refused(s.port, &s.admin, "box", &work, &full(1, 0, 1))?;

    // The finished one, sent a message while the other works, waits for room, saying so: its
    // message waits in its queue, and a new one is refused while it waits.
    command(s.port, &s.admin, &first, &json!({"type": "send", "id": "and-then", "text": "and then the docs"}))?;
    until(s.port, &s.admin, "box", "had one at work and one waiting for room", &log, |v| (v.get("working").and_then(Value::as_u64), v.get("waiting").and_then(Value::as_u64)) == (Some(1), Some(1)))?;
    let waits = session_until(s.port, &s.admin, &first, "waited for room", &log, |v| v.get("room_wait").is_some_and(|w| !w.is_null()))?;
    assert_eq!((waits.get("status").and_then(Value::as_str), waits.get("queued").and_then(Value::as_u64)), (Some("idle"), Some(1)), "{waits}");
    refused(s.port, &s.admin, "box", &work, &full(1, 1, 1))?;
    // The other's turn over, there's room: the message goes, and it's at work (on a turn that goes
    // on for as long as the test does).
    std::fs::write(&go, "")?;
    session_until(s.port, &s.admin, &first, "went to work", &log, |v| v.get("status").and_then(Value::as_str) == Some("running") && v.get("room_wait").is_none_or(Value::is_null))?;
    until(s.port, &s.admin, "box", "had one at work and none waiting", &log, |v| (v.get("working").and_then(Value::as_u64), v.get("waiting").and_then(Value::as_u64)) == (Some(1), Some(0)))?;
    std::fs::remove_file(&go)?;

    // Set to 3 on the server, which wins over its config: two more at work, then a fourth refused.
    let (st, v) = put(s.port, &s.admin, "box", &json!({"max_sessions": 3}))?;
    assert_eq!((st, v.get("max_sessions").and_then(Value::as_u64)), (200, Some(3)), "{v}");
    let v = holds(s.port, &s.admin, "box", (3, "server"), &log)?;
    assert_eq!(v.pointer("/held/config").and_then(Value::as_u64), Some(1), "its config's beside it: {v}");
    for _ in 0..2 {
        ids.push(opened(s.port, &s.admin, "box", &work)?);
    }
    working(s.port, &s.admin, "box", 3, &log)?;
    refused(s.port, &s.admin, "box", &work, &full(3, 0, 3))?;
    assert!(std::fs::read_to_string(&kept)?.contains('3'), "kept on the machine for its restarts");

    // Taken away on the server: its config's 1 holds again, those at work go on, a new one waits.
    let (st, v) = put(s.port, &s.admin, "box", &json!({"max_sessions": null}))?;
    assert_eq!((st, v.get("max_sessions")), (200, Some(&Value::Null)), "{v}");
    holds(s.port, &s.admin, "box", (1, "config"), &log)?;
    assert!(!kept.exists(), "nothing set: nothing kept");
    working(s.port, &s.admin, "box", 3, &log)?;
    refused(s.port, &s.admin, "box", &work, &full(3, 0, 1))?;

    // Its config, changed: held within seconds, nothing restarting.
    std::fs::write(&config, "[agent]\nmax_agent_sessions = 5\n")?;
    holds(s.port, &s.admin, "box", (5, "config"), &log)?;
    ids.push(opened(s.port, &s.admin, "box", &work)?);
    working(s.port, &s.admin, "box", 4, &log)?;

    // What isn't a limit is refused, saying why, and leaves it as it was; a runner the server
    // doesn't know has none.
    for bad in [json!({"max_sessions": 0}), json!({"max_sessions": 5000}), json!({"max_session": 3}), json!({"max_sessions": "lots"}), json!({})] {
        let (st, v) = put(s.port, &s.admin, "box", &bad)?;
        assert_eq!(st, 400, "{bad}: {v}");
    }
    assert_eq!(view(s.port, &s.admin, "box")?.get("max_sessions"), Some(&Value::Null));
    let (st, _) = http(s.port, "GET", "/api/runners/nobody/agent-limit", &s.admin, None)?;
    assert_eq!(st, 404);
    let (st, _) = put(s.port, &s.admin, "nobody", &json!({"max_sessions": 3}))?;
    assert_eq!(st, 404);

    // Set to 3 on the server again, and its turns let finish (none is left waiting for `go`), the
    // long one stopped by force (the stand-in goes on with a turn it's asked to stop).
    let (st, _) = put(s.port, &s.admin, "box", &json!({"max_sessions": 3}))?;
    assert_eq!(st, 200);
    holds(s.port, &s.admin, "box", (3, "server"), &log)?;
    std::fs::write(&go, "")?;
    command(s.port, &s.admin, &first, &json!({"type": "force_stop"}))?;
    working(s.port, &s.admin, "box", 0, &log)?;
    let all = view(s.port, &s.admin, "box")?.get("sessions").and_then(Value::as_u64).unwrap_or_default();
    assert!(all >= 5, "every session it has, finished ones too: {all}");

    // Started again: it holds 3 as it starts, before it's connected, and every session it ran comes
    // back.
    let before = std::fs::read_to_string(&log)?.len();
    drop(runner);
    let (runner, log) = agents::runner_with(dir.path(), s.port, "box", None, TALK, &env)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let after = loop {
        let text = std::fs::read_to_string(&log)?;
        let after = text.get(before..).unwrap_or_default().to_owned();
        if after.contains("connected to server") {
            break after;
        }
        anyhow::ensure!(Instant::now() < deadline, "the runner never connected again\n{}", tail(&log));
        std::thread::sleep(Duration::from_millis(200));
    };
    let held_at = after.lines().position(|l| l.contains("at most this many at work at once") && l.contains("max_sessions=3") && l.contains("Server"));
    let connected_at = after.lines().position(|l| l.contains("connected to server"));
    assert!(held_at.is_some() && held_at < connected_at, "it held 3 before it connected:\n{after}");
    agents::connected(s.port, &s.admin, "box", &log, Duration::from_secs(120))?;
    holds(s.port, &s.admin, "box", (3, "server"), &log)?;
    let deadline = Instant::now() + Duration::from_secs(90);
    for id in &ids {
        loop {
            let (_, body) = http(s.port, "GET", &format!("/api/harness/sessions/{id}"), &s.admin, None)?;
            let status = serde_json::from_str::<Value>(&body).ok().and_then(|v| v.get("status").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
            if matches!(status.as_str(), "starting" | "idle" | "running" | "waiting") {
                break;
            }
            anyhow::ensure!(Instant::now() < deadline, "{id} didn't come back: {body}\n{}", tail(&log));
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    drop(runner);
    Ok(())
}
