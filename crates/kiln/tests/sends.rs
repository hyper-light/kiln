//! CP-T3.2 through a runner: a message sent twice under one key reaches its agent once, and both
//! sends are answered with what became of it. A runner of its own (`kiln agent`, connected over
//! QUIC like any other) holds the agent: the server sends the message as `HarnessSend`, the
//! runner's host takes it down the session's lane through its inbox, and says what became of it
//! over its link. The agent's harness is a stand-in for Claude Code that keeps what it's sent.
#![cfg(unix)]

mod common;

use std::process::Child;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The prompts the stand-in harness was sent, by their message ids.
fn prompts(heard: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(heard)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("type") == Some(&json!("user")))
        .filter_map(|v| v.get("uuid").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// A request as `common::http` makes it, under an `Idempotency-Key` header, with no body.
fn keyed(port: u16, method: &str, path: &str, token: &str, key: &str) -> anyhow::Result<(u16, Value)> {
    use std::io::{Read as _, Write as _};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    write!(s, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nIdempotency-Key: {key}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}")?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status: u16 = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { common::dechunk(rest) } else { rest.to_owned() };
    Ok((status, serde_json::from_str(&body).unwrap_or(Value::Null)))
}

/// A runner process, stopped with the test.
struct Runner(Child);

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_message_sent_twice_under_one_key_through_a_runner_reaches_its_agent_once() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir()?;
    let server = common::start(dir.path(), &common::Options::default())?;
    // The stand-in harness, on the runner's PATH: it says it started, then keeps what it's sent.
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin)?;
    let (heard, started) = (dir.path().join("heard.jsonl"), dir.path().join("started"));
    let claude = bin.join("claude");
    std::fs::write(&claude, format!("#!/bin/sh\ntouch '{}'\nexec cat >> '{}'\n", started.display(), heard.display()))?;
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755))?;
    // A runner of its own, joined with the server's token.
    let join = std::fs::read_to_string(dir.path().join("data").join("join.token"))?.trim().to_owned();
    let (home, state, work) = (dir.path().join("runner-home"), dir.path().join("runner-state"), dir.path().join("runner-work"));
    for d in [&home, &state, &work] {
        std::fs::create_dir_all(d)?;
    }
    let log = std::fs::File::create(dir.path().join("runner.log"))?;
    let mut cmd = common::kiln();
    cmd.args(["agent", "--server", &format!("http://127.0.0.1:{}", server.port), "--token", &join, "--name", "t3-runner", "--labels", "t3"])
        .arg("--state-dir")
        .arg(&state)
        .arg("--work-dir")
        .arg(&work)
        .env("HOME", &home)
        .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()))
        .env("NO_COLOR", "1")
        .stdout(log.try_clone()?)
        .stderr(log);
    // A runner started by a blue/green upgrade passes its own on to what it runs: a `kiln agent` that
    // has them takes itself for a new build taking over, and refuses every message `moved`.
    for var in ["KILN_CONFIG", "KILN_URL", "KILN_ADMIN_TOKEN", "KILN_SUPERVISED", "KILN_GENERATION", "KILN_LOG", "KILN_SERVER", "KILN_JOIN_TOKEN", "KILN_AGENT_NAME", "KILN_STATE_DIR", "KILN_INSTANCE", "KILN_TAKE_OVER_FROM", "KILN_UPGRADE_ID", "KILN_SUPERVISOR"] {
        cmd.env_remove(var);
    }
    let _runner = Runner(cmd.spawn()?);
    let runner_log = || std::fs::read_to_string(dir.path().join("runner.log")).unwrap_or_default().lines().rev().take(40).collect::<Vec<_>>().join("\n");
    let api = |method: &str, at: &str, body: Option<&Value>| -> anyhow::Result<(u16, Value)> {
        let (status, text) = common::http(server.port, method, at, &server.admin, body)?;
        Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
    };
    let wait = |what: &str, done: &mut dyn FnMut() -> anyhow::Result<bool>| -> anyhow::Result<()> {
        let until = Instant::now() + Duration::from_secs(90);
        while !done()? {
            anyhow::ensure!(Instant::now() < until, "{what}, not in 90 s.\nserver: {}\nrunner (newest first): {}", server.log(), runner_log());
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    };
    wait("the runner connected", &mut || {
        let (_, runners) = api("GET", "/api/agents", None)?;
        Ok(runners.as_array().into_iter().flatten().any(|r| r.get("name") == Some(&json!("t3-runner")) && r.get("connected") == Some(&json!(true))))
    })?;
    // An agent on it, its harness running there.
    let cwd = dir.path().join("agent");
    std::fs::create_dir_all(&cwd)?;
    let (status, made) = api("POST", "/api/harness/agents", Some(&json!({"kind": "claude", "on": "t3-runner", "cwd": cwd})))?;
    anyhow::ensure!(status == 200, "starting an agent on the runner: {status} {made}");
    let id = made.pointer("/agents/0/id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no agent: {made}"))?.to_owned();
    wait("its harness started on the runner", &mut || Ok(started.exists()))?;
    // Sent twice under one key: the same message, answered alike, the second a repeat.
    let send = || api("POST", &format!("/api/harness/agents/{id}/send"), Some(&json!({"text": "deploy the fix", "key": "deploy-1"})));
    let (s1, first) = send()?;
    let (s2, again) = send()?;
    anyhow::ensure!(s1 == 200 && s2 == 200, "{s1} {first} / {s2} {again}");
    let message = first.get("message").and_then(Value::as_str).unwrap_or_default().to_owned();
    anyhow::ensure!(!message.is_empty() && again.get("message") == first.get("message"), "the same message: {first} {again}");
    let answer = |v: &Value, k: &str| v.pointer(&format!("/accepted/{k}")).cloned();
    for k in ["seq", "at", "epoch", "session", "message"] {
        anyhow::ensure!(answer(&first, k).is_some() && answer(&first, k) == answer(&again, k), "{k}: {first} {again}");
    }
    anyhow::ensure!(answer(&first, "repeat").is_none() && answer(&again, "repeat") == Some(json!(true)), "{first} {again}");
    // One prompt reached the harness on the runner.
    wait("the harness got its prompt", &mut || Ok(!prompts(&heard).is_empty()))?;
    std::thread::sleep(Duration::from_millis(500));
    anyhow::ensure!(prompts(&heard) == [message.clone()], "the harness was sent {:?}", prompts(&heard));
    // The goals panel's Add asked again under its key (its answer lost): the goal is added once.
    let add = || api("POST", &format!("/api/harness/sessions/{id}/goals"), Some(&json!({"goals": ["the fix is deployed"], "start": false, "key": "goal-1"})));
    let (g1, _) = add()?;
    let (g2, stack) = add()?;
    let added = stack.get("items").and_then(Value::as_array).map(|items| items.iter().filter(|i| i.get("text") == Some(&json!("the fix is deployed"))).count());
    anyhow::ensure!(g1 == 200 && g2 == 200 && added == Some(1), "goals added twice under one key: {g1} / {g2} {stack}");
    // The twelfth adversarial check's: a skip naming no goal, under its `Idempotency-Key` header,
    // asked again (its answer lost) skips no more (a person's skip without a key, asked again, would
    // skip the next goal too).
    let (st, _) = api("POST", &format!("/api/harness/sessions/{id}/goals/start"), Some(&json!({})))?;
    let (more, _) = api("POST", &format!("/api/harness/sessions/{id}/goals"), Some(&json!({"goals": ["the docs say so"], "start": false, "key": "goal-2"})))?;
    anyhow::ensure!(st == 200 && more == 200, "the stack started ({st}), a goal added ({more})");
    let skip = || keyed(server.port, "POST", &format!("/api/harness/sessions/{id}/goals/skip"), &server.admin, "skip-1");
    let (k1, _) = skip()?;
    let (k2, stack) = skip()?;
    let status = |text: &str| stack.get("items").and_then(Value::as_array).and_then(|items| items.iter().find(|i| i.get("text") == Some(&json!(text)))).and_then(|i| i.get("status")).cloned();
    anyhow::ensure!(
        k1 == 200 && k2 == 200 && status("the fix is deployed") == Some(json!("skipped")) && status("the docs say so") == Some(json!("active")),
        "a skip asked again under its key: {k1} / {k2} {stack}"
    );
    Ok(())
}
