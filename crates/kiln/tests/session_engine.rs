//! Each session's engine (the person's decision of 2026-11-23: each agent session chooses it, never its runner; docs/
//! isolation.md, "Each session's engine"), end to end through kiln's API, MCP and code mode: a server, and a runner of
//! this build that can't run microVMs, its state folder holding a file where kiln writes the Shards it carries out, so it
//! holds none whatever its host. Its facts say it can't, and why, and name no engine of its own; no route or operation
//! sets a runner's engine. A session that asks for a microVM is refused there, typed (409), in the runner's words, on
//! every surface, and nothing starts; one that asks for none starts on its host. A runner that can run microVMs, and
//! placement between two runners, boot real microVMs: `vm_harnesses`' `engine::` tests.
//!
//! `cargo test -p kiln --test session_engine`

#![cfg(unix)]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, Server, http, machine};
use serde_json::{Value, json};

const TALK: Talk = Talk { lines: 2, every_ms: 50 };

fn call(s: &Server, method: &str, path: &str, body: Option<Value>) -> anyhow::Result<(u16, Value)> {
    let (st, text) = http(s.port, method, path, &s.admin, body.as_ref())?;
    Ok((st, serde_json::from_str(&text).unwrap_or(Value::String(text))))
}

/// One MCP tool call: whether it failed, and what it said.
fn mcp(s: &Server, tool: &str, arguments: Value) -> anyhow::Result<(bool, String)> {
    let (st, body) = http(s.port, "POST", "/api/mcp", &s.admin, Some(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": tool, "arguments": arguments } })))?;
    anyhow::ensure!(st == 200, "{tool}: {st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    Ok((v.pointer("/result/isError").and_then(Value::as_bool).unwrap_or(true), v.pointer("/result/content/0/text").and_then(Value::as_str).unwrap_or_default().to_owned()))
}

/// The end of a runner's log, to say why a test failed.
fn tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines.iter().rev().take(40).rev().copied().collect::<Vec<_>>().join("\n")
}

/// Waits until `look` says what `holds` wants, within a minute.
fn until<T>(what: &str, log: &Path, mut look: impl FnMut() -> anyhow::Result<T>, holds: impl Fn(&T) -> bool) -> anyhow::Result<T>
where
    T: std::fmt::Debug,
{
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let v = look()?;
        if holds(&v) {
            return Ok(v);
        }
        anyhow::ensure!(Instant::now() < deadline, "never {what}: {v:?}\n{}", tail(log));
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// The runner's entry in the runners API (`GET /api/agents`).
fn listed(s: &Server, name: &str) -> anyhow::Result<Value> {
    let (st, v) = call(s, "GET", "/api/agents", None)?;
    anyhow::ensure!(st == 200, "GET /api/agents: {st} {v}");
    let key = machine(name);
    Ok(v.as_array().and_then(|a| a.iter().find(|r| r.get("name").and_then(Value::as_str) == Some(key.as_str())).cloned()).unwrap_or(Value::Null))
}

/// Waits until session `id`, as the server has it, is as `holds` says.
fn session_until(s: &Server, id: &str, what: &str, log: &Path, holds: impl Fn(&Value) -> bool) -> anyhow::Result<Value> {
    until(what, log, || Ok(call(s, "GET", &format!("/api/harness/sessions/{id}"), None)?.1), holds)
}

/// How many sessions the server has.
fn sessions(s: &Server) -> anyhow::Result<usize> {
    Ok(call(s, "GET", "/api/harness/sessions", None)?.1.as_array().map_or(0, Vec::len))
}

/// The person's decision of 2026-11-23, on a runner that can't run microVMs. Its facts say so in the runners API
/// (`virt.microvm` false, `isolation.why_not` its why) and name no engine of its own (`isolation.engine` absent); nothing
/// sets a runner's engine (`/api/runners/:name/engine` is no route, `runners_engine_set` no tool). A session that asks
/// for a microVM is refused, typed (409, `<runner> can't run microVMs: <why>`), by `agents.start` placed by need and
/// naming the runner, by code mode, and by the console's route, and nothing starts; one that asks for none starts on its
/// host, with no microVM, by the console's route and by `agents.start`.
#[test]
fn a_runner_that_cant_run_microvms_refuses_a_microvm_session_typed_and_runs_one_on_its_host() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = common::start(dir.path(), &Options::default())?;
    std::fs::create_dir_all(dir.path().join("box-state"))?;
    std::fs::write(dir.path().join("box-state/shards"), "not a folder")?;
    let (runner, log) = agents::runner_with(dir.path(), s.port, "box", None, TALK, &[])?;
    agents::connected(s.port, &s.admin, "box", &log, Duration::from_secs(120))?;
    agents::headroom_off(s.port, &s.admin, "box", Duration::from_secs(60))?;

    // Its facts: what it can do, and why not, with no engine of its own.
    let r = until("said what it can do", &log, || listed(&s, "box"), |r| r.pointer("/facts/isolation").is_some())?;
    let why = r.pointer("/facts/isolation/why_not").and_then(Value::as_str).unwrap_or_default().to_owned();
    assert!(!why.is_empty(), "its facts say why it can't run microVMs: {r}");
    assert_eq!((r.pointer("/facts/isolation/available"), r.pointer("/facts/virt/microvm"), r.pointer("/facts/isolation/engine")), (Some(&json!(false)), Some(&json!(false)), None), "{r}");

    // Nothing sets a runner's engine.
    for (method, body) in [("GET", None), ("PUT", Some(json!({"engine": "microvm"})))] {
        let (st, v) = call(&s, method, &format!("/api/runners/{}/engine", machine("box")), body)?;
        assert_eq!(st, 404, "{method} /api/runners/:name/engine: {v}");
    }
    let (failed, said) = mcp(&s, "runners_engine_set", json!({"runner": machine("box"), "engine": "microvm"}))?;
    assert!(failed, "no tool sets a runner's engine: {said}");

    // A session that asks for a microVM: refused, typed, in its words, wherever it's asked; nothing starts.
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    let cwd = work.display().to_string();
    let before = sessions(&s)?;
    let (failed, said) = mcp(&s, "agents_start", json!({"kind": "claude", "cwd": cwd, "isolation": "microvm"}))?;
    assert!(failed && said.contains("can't run microVMs: ") && said.contains(&why), "agents.start by need: {said}");
    let (failed, said) = mcp(&s, "agents_start", json!({"kind": "claude", "cwd": cwd, "isolation": "microvm", "on": machine("box")}))?;
    assert!(failed && said.contains("can't run microVMs: ") && said.contains(&why), "agents.start on the runner: {said}");
    let program = "return await kiln.agents.start({ kind: 'claude', cwd: input.cwd, isolation: 'microvm', on: input.runner })";
    let (failed, said) = mcp(&s, "code_run", json!({"program": program, "input": {"cwd": cwd, "runner": machine("box")}}))?;
    assert!(failed && said.contains("can't run microVMs: "), "code mode: {said}");
    let open = json!({"target": format!("agent:{}", machine("box")), "spec": {"kind": "claude", "cwd": cwd, "isolation": {"kind": "microvm"}}});
    let (st, said) = common::http_within(s.port, "POST", "/api/harness/sessions", &s.admin, Some(&open), Duration::from_secs(75))?;
    assert!(st == 409 && said.contains("can't run microVMs: ") && said.contains(&why), "the console's route: {st} {said}");
    assert_eq!(sessions(&s)?, before, "nothing started");

    // One that asks for none: on its host, with no microVM, by the console's route and by `agents.start`.
    let id = agents::open_session(s.port, &s.admin, "box", &work, Duration::from_secs(90))?;
    let info = session_until(&s, &id, "said where it runs", &log, |v| v.pointer("/runs/in").is_some())?;
    assert!(info.pointer("/runs/in") == Some(&json!("host")) && info.get("isolation").is_none_or(Value::is_null), "on its host, no microVM: {info}");
    let (failed, said) = mcp(&s, "agents_start", json!({"kind": "claude", "cwd": cwd, "on": machine("box"), "prompt": "stream"}))?;
    assert!(!failed, "{said}");
    let started: Value = serde_json::from_str(&said).unwrap_or_default();
    let id = started.pointer("/agents/0/id").or_else(|| started.pointer("/0/id")).and_then(Value::as_str).unwrap_or_default().to_owned();
    anyhow::ensure!(!id.is_empty(), "no id in {said}");
    let info = session_until(&s, &id, "said where it runs", &log, |v| v.pointer("/runs/in").is_some())?;
    assert!(info.pointer("/runs/in") == Some(&json!("host")) && info.get("isolation").is_none_or(Value::is_null), "on its host, no microVM: {info}");
    drop(runner);
    Ok(())
}
