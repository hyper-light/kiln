//! A runner's engine (kiln plan 01a1dd2e…, P2.3 and P2.4; docs/isolation.md, "A runner's engine"), end
//! to end through kiln's API, MCP and code mode: a server, and a runner of this build, which carries no
//! Shards and so can't hold agents in microVMs. It says `host` and why, and `microvm` is refused, saying
//! why, on every surface. Its owner sets `host` and takes it away, and the runner keeps what was set for
//! its restarts. A session that asks nothing starts on its host, with no microVM. One that needs a microVM
//! is refused, the engine named (P2.4 AC2). The runner side of `microvm` (a runner that holds microVMs
//! defaulting to it, a switch to `host`, placement between two runners) needs a kiln carrying Shards:
//! `vm_harnesses`' `engine::` tests.
//!
//! `cargo test -p kiln --test runner_engine`

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

/// The runner's engine, as `GET /api/runners/:name/engine` says it.
fn engine(s: &Server, name: &str) -> anyhow::Result<Value> {
    let (st, v) = call(s, "GET", &format!("/api/runners/{}/engine", machine(name)), None)?;
    anyhow::ensure!(st == 200, "GET its engine: {st} {v}");
    Ok(v)
}

/// The runner's entry in the runners API (`GET /api/agents`).
fn listed(s: &Server, name: &str) -> anyhow::Result<Value> {
    let (st, v) = call(s, "GET", "/api/agents", None)?;
    anyhow::ensure!(st == 200, "GET /api/agents: {st} {v}");
    let key = machine(name);
    Ok(v.as_array().and_then(|a| a.iter().find(|r| r.get("name").and_then(Value::as_str) == Some(key.as_str())).cloned()).unwrap_or(Value::Null))
}

fn put(s: &Server, name: &str, body: Value) -> anyhow::Result<(u16, Value)> {
    call(s, "PUT", &format!("/api/runners/{}/engine", machine(name)), Some(body))
}

/// P2.3 AC1, AC2 and AC4, and its third test: a runner that can't hold microVMs shows `host` and why, in
/// the runners API and its engine's own view, its isolation facts beside it; `microvm` is refused on it,
/// saying why, through REST, MCP and code mode, and leaves it as it was; its owner sets `host` (kept on
/// the runner for its restarts) and takes it away. What isn't an engine is refused, saying why; a runner
/// the server doesn't know has none.
#[test]
fn p2_3_a_runner_without_microvms_runs_host_says_why_and_cant_be_set_to_microvm() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = common::start(dir.path(), &Options::default())?;
    let (runner, log) = agents::runner_with(dir.path(), s.port, "box", None, TALK, &[])?;
    agents::connected(s.port, &s.admin, "box", &log, Duration::from_secs(120))?;
    let kept = dir.path().join("box-state/runner-engine.json");

    // AC2: `host`, from what it can do, and why: its own facts' why. AC4: its isolation facts with it.
    let v = until("said its engine", &log, || engine(&s, "box"), |v| v.get("engine").is_some_and(|e| !e.is_null()))?;
    let why = v.pointer("/isolation/why_not").and_then(Value::as_str).unwrap_or_default().to_owned();
    assert!(!why.is_empty(), "its facts say why it holds no microVM: {v}");
    assert_eq!((v.get("engine"), v.get("from"), v.get("asked")), (Some(&json!("host")), Some(&json!("default")), Some(&Value::Null)), "{v}");
    assert_eq!((v.get("why").and_then(Value::as_str), v.pointer("/microvm/usable")), (Some(why.as_str()), Some(&json!(false))), "{v}");
    assert_eq!((v.pointer("/isolation/available"), v.pointer("/isolation/engine"), v.pointer("/virt/microvm")), (Some(&json!(false)), Some(&json!("host")), Some(&json!(false))), "{v}");
    // AC1: the runners API says it too, in its facts.
    let r = until("listed its engine", &log, || listed(&s, "box"), |r| r.pointer("/facts/isolation/engine").is_some())?;
    assert_eq!((r.pointer("/facts/isolation/engine"), r.pointer("/facts/virt/microvm")), (Some(&json!("host")), Some(&json!(false))), "{r}");
    let (failed, said) = mcp(&s, "runners_engine", json!({"runner": machine("box")}))?;
    assert!(!failed && (said.contains("\"engine\": \"host\"") || said.contains("\"engine\":\"host\"")), "MCP reads it: {said}");

    // Its third test: `microvm` refused, saying why, on every surface; nothing changes.
    let (st, v) = put(&s, "box", json!({"engine": "microvm"}))?;
    assert!(st == 409 && v.to_string().contains("can't run its sessions in microVMs") && v.to_string().contains(&why), "REST: {st} {v}");
    let (failed, said) = mcp(&s, "runners_engine_set", json!({"runner": machine("box"), "engine": "microvm"}))?;
    assert!(failed && said.contains("can't run its sessions in microVMs") && said.contains(&why), "MCP: {said}");
    let (failed, said) = mcp(&s, "code_run", json!({"program": "return await kiln.runners.engine.set({ runner: input.runner, engine: 'microvm' })", "input": {"runner": machine("box")}}))?;
    assert!(failed && said.contains("can't run its sessions in microVMs"), "code mode: {said}");
    assert_eq!((engine(&s, "box")?.get("asked"), engine(&s, "box")?.get("version")), (Some(&Value::Null), Some(&json!(0))), "a refusal sets nothing");

    // Its owner sets `host`: held, from the server, and kept on the runner for its restarts.
    let (st, v) = put(&s, "box", json!({"engine": "host"}))?;
    assert_eq!((st, v.get("engine"), v.get("from"), v.get("asked"), v.get("version")), (200, Some(&json!("host")), Some(&json!("server")), Some(&json!("host")), Some(&json!(1))), "{v}");
    until("kept host on the runner", &log, || Ok(std::fs::read_to_string(&kept).unwrap_or_default()), |t| t.contains("host") && t.contains("\"version\":1"))?;
    // Decision 8, with nothing to drain: the list says none and no switch drains; "End them now" ends nothing, and
    // the switch is done, so cancelling it is refused (REST, MCP and code mode); a runner kiln doesn't know is 404.
    assert_eq!((v.get("draining_sessions"), v.get("switch")), (Some(&json!([])), Some(&Value::Null)), "{v}");
    let (st, v) = call(&s, "POST", &format!("/api/runners/{}/engine/end", machine("box")), None)?;
    assert!(st == 200 && v.get("ended") == Some(&json!([])) && v.get("engine") == Some(&json!("host")), "End them now: {st} {v}");
    let (st, v) = call(&s, "POST", &format!("/api/runners/{}/engine/cancel", machine("box")), None)?;
    assert!(st == 409 && v.to_string().contains("drains: its switch to host is done"), "cancel: {st} {v}");
    let (failed, said) = mcp(&s, "runners_engine_end_draining", json!({"runner": machine("box")}))?;
    assert!(!failed && said.contains("\"ended\""), "MCP's End them now: {said}");
    let (failed, said) = mcp(&s, "code_run", json!({"program": "return await kiln.runners.engine.cancel({ runner: input.runner })", "input": {"runner": machine("box")}}))?;
    assert!(failed && said.contains("its switch to host is done"), "code mode's cancel: {said}");
    for what in ["end", "cancel"] {
        let (st, _) = call(&s, "POST", &format!("/api/runners/nobody/engine/{what}"), None)?;
        assert_eq!(st, 404, "{what}");
    }
    assert_eq!(engine(&s, "box")?.get("version"), Some(&json!(1)), "neither set anything");
    let (failed, said) = mcp(&s, "code_run", json!({"program": "return (await kiln.runners.engine({ runner: input.runner })).from", "input": {"runner": machine("box")}}))?;
    assert!(!failed && said.contains("server"), "code mode reads it: {said}");
    // Taken away through MCP: what it can do decides again, and the runner keeps nothing.
    let (failed, said) = mcp(&s, "runners_engine_set", json!({"runner": machine("box"), "engine": null}))?;
    assert!(!failed, "{said}");
    assert_eq!((engine(&s, "box")?.get("from"), engine(&s, "box")?.get("asked"), engine(&s, "box")?.get("version")), (Some(&json!("default")), Some(&Value::Null), Some(&json!(2))));
    until("kept nothing set, at setting 2", &log, || Ok(std::fs::read_to_string(&kept).unwrap_or_default()), |t| t.contains("\"engine\":null") && t.contains("\"version\":2"))?;

    // What isn't an engine; a runner the server doesn't know.
    for bad in [json!({}), json!({"engine": "docker"}), json!({"engin": "host"}), json!({"engine": 1}), json!(["host"])] {
        let (st, v) = put(&s, "box", bad.clone())?;
        assert_eq!(st, 400, "{bad}: {v}");
    }
    let (st, _) = call(&s, "GET", "/api/runners/nobody/engine", None)?;
    assert_eq!(st, 404);
    let (st, _) = put(&s, "nobody", json!({"engine": "host"}))?;
    assert_eq!(st, 404);
    drop(runner);
    Ok(())
}


/// Waits until session `id`, as the server has it, is as `holds` says.
fn session_until(s: &Server, id: &str, what: &str, log: &Path, holds: impl Fn(&Value) -> bool) -> anyhow::Result<Value> {
    until(what, log, || Ok(call(s, "GET", &format!("/api/harness/sessions/{id}"), None)?.1), holds)
}

/// P2.3's second test on a `host` runner, and P2.4's second: a new session that asks nothing starts on
/// the runner's host with no microVM, by the console's route and by `agents.start` alike, and counts
/// among those on its engine; one that needs a microVM is refused with words naming the engine, by
/// `agents.start` (placed by need, every runner here `host`) and by a start naming the runner; one
/// asked as a process (`isolation` "none") starts there.
#[test]
fn p2_3_a_new_session_takes_its_runners_engine_and_p2_4_a_microvm_one_is_refused_on_host_runners() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = common::start(dir.path(), &Options::default())?;
    let (runner, log) = agents::runner_with(dir.path(), s.port, "box", None, TALK, &[])?;
    agents::connected(s.port, &s.admin, "box", &log, Duration::from_secs(120))?;
    agents::headroom_off(s.port, &s.admin, "box", Duration::from_secs(60))?;
    until("said its engine", &log, || engine(&s, "box"), |v| v.get("engine") == Some(&json!("host")))?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    let cwd = work.display().to_string();

    // Asking nothing: on its host, no microVM; on its engine, none draining.
    let id = agents::open_session(s.port, &s.admin, "box", &work, Duration::from_secs(90))?;
    let info = session_until(&s, &id, "said where it runs", &log, |v| v.pointer("/runs/in").is_some())?;
    assert!(info.pointer("/runs/in") == Some(&json!("host")) && info.get("isolation").is_none_or(Value::is_null), "on its host, no microVM: {info}");
    let v = until("counted it", &log, || engine(&s, "box"), |v| v.get("sessions").and_then(Value::as_u64) >= Some(1))?;
    assert_eq!(v.get("draining"), Some(&json!(0)), "{v}");

    // Needing a microVM: refused, the engine named, by need and by naming the runner; nothing starts.
    let before = call(&s, "GET", "/api/harness/sessions", None)?.1.as_array().map_or(0, Vec::len);
    let (failed, said) = mcp(&s, "agents_start", json!({"kind": "claude", "cwd": cwd, "isolation": "microvm"}))?;
    assert!(failed && said.contains("needs a microVM starts only on a runner whose engine is microvm") && said.contains("its engine is host"), "agents.start by need: {said}");
    let (failed, said) = mcp(&s, "agents_start", json!({"kind": "claude", "cwd": cwd, "isolation": "microvm", "on": machine("box")}))?;
    assert!(failed && said.contains("whose engine is microvm") && said.contains("its engine is host"), "agents.start on the runner: {said}");
    let open = json!({"target": format!("agent:{}", machine("box")), "spec": {"kind": "claude", "cwd": cwd, "isolation": {"kind": "microvm"}}});
    let (st, said) = common::http_within(s.port, "POST", "/api/harness/sessions", &s.admin, Some(&open), Duration::from_secs(75))?;
    assert!(st >= 400 && said.contains("its engine is host"), "the console's route: {st} {said}");
    assert_eq!(call(&s, "GET", "/api/harness/sessions", None)?.1.as_array().map_or(0, Vec::len), before, "nothing started");

    // Asked as a process: it starts there, on its host.
    let (failed, said) = mcp(&s, "agents_start", json!({"kind": "claude", "cwd": cwd, "isolation": "none", "on": machine("box"), "prompt": "stream"}))?;
    assert!(!failed, "{said}");
    let started: Value = serde_json::from_str(&said).unwrap_or_default();
    let id = started.pointer("/agents/0/id").or_else(|| started.pointer("/0/id")).and_then(Value::as_str).unwrap_or_default().to_owned();
    anyhow::ensure!(!id.is_empty(), "no id in {said}");
    let info = session_until(&s, &id, "said where it runs", &log, |v| v.pointer("/runs/in").is_some())?;
    assert_eq!(info.pointer("/runs/in"), Some(&json!("host")), "{info}");
    drop(runner);
    Ok(())
}
