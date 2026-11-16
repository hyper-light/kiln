//! Agent definitions kept in kiln (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P5.1), through each
//! surface people and agents reach them by: the routes, the operations over HTTP, MCP and code mode.
//! AC1: a person creates, versions, restores and deletes agents (one of the ten harnesses, model,
//! instructions, tools and MCP, skills, limits, Agentfile) on every surface; AC2: others can't see a
//! person's agents. Settings kiln can't run (an unknown harness, an Agentfile its owner doesn't reach)
//! aren't kept. A run's start is `step32`'s lab's (`src/reach/tests/doors.rs`, P5.1's run test):
//! no machine here starts microVMs.
//!
//! `cargo test -p kiln --test agentdefs`

#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, Server, http, machine, raw, start_env};
use serde_json::{Value, json};

const CLAUDE: &str = "FROM ubuntu:24.04\nHARNESS claude FROM ./harness\nAGENT main FROM ./agent\nATTACH main FOR claude\nCMD [\"claude\"]\n";

/// A person, `name`, with `grants`, and a token of theirs that does all they may.
fn person(s: &Server, name: &str, grants: &[&str]) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": grants })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}'s token: {st} {body}");
    serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))
}

fn call(s: &Server, token: &str, method: &str, path: &str, body: Option<Value>) -> anyhow::Result<(u16, Value)> {
    let (st, text) = http(s.port, method, path, token, body.as_ref())?;
    Ok((st, serde_json::from_str(&text).unwrap_or(Value::String(text))))
}

/// One MCP tool call with `token`: whether it failed, and what it said.
fn mcp(s: &Server, token: &str, tool: &str, arguments: Value) -> anyhow::Result<(bool, String)> {
    let (st, body) = http(s.port, "POST", "/api/mcp", token, Some(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": tool, "arguments": arguments } })))?;
    anyhow::ensure!(st == 200, "{tool}: {st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    Ok((v.pointer("/result/isError").and_then(Value::as_bool).unwrap_or(true), v.pointer("/result/content/0/text").and_then(Value::as_str).unwrap_or_default().to_owned()))
}

fn s(v: &Value, pointer: &str) -> String {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// P5.1 AC1 and AC2: Ada keeps an agent with every setting, changes it (a new version; a rename alone
/// none), restores an earlier one as a new version and lists them, on REST, her MCP tools and code
/// mode; Bo reaches none of hers on any surface (404, as no agent definition) and lists only his own;
/// an admin reaches everyone's; Ada deletes hers, its versions with it. Taking out `Who::reaches`'
/// owner rule passes Bo's reads; making a rename a version breaks the versions' list.
#[test]
fn p5_1_ac1_ac2_agent_definitions_round_trip_and_stay_their_owners() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let srv = start_env(dir.path(), &Options::default(), false, &[])?;
    let ada = person(&srv, "ada", &["developer"])?;
    let bo = person(&srv, "bo", &["developer"])?;
    let root = person(&srv, "root", &["admin"])?;

    // Her Agentfile, for the agent to run in.
    let (st, af) = call(&srv, &ada, "POST", "/api/agentfiles", Some(json!({ "name": "claude-rust", "text": CLAUDE })))?;
    anyhow::ensure!(st == 200, "{st} {af}\n{}", srv.log());
    let agentfile = format!("agentfile:{}", s(&af, "/agentfile/id"));

    // Kept, version 1, every setting as given.
    let v1 = json!({
        "name": "reviewer",
        "harness": "claude",
        "instructions": "Review the diff; report findings by file and line.",
        "tools": ["Read", "Grep"],
        "mcp": [{ "name": "kiln", "url": "https://kiln.example/api/mcp" }, { "name": "local", "command": ["mcp-local", "--stdio"] }],
        "skills": ["code-review"],
        "limits": { "seconds": 1800, "cents": 500, "tokens": 2000000 },
        "agentfile": agentfile,
    });
    let (st, made) = call(&srv, &ada, "POST", "/api/agentdefs", Some(v1.clone()))?;
    anyhow::ensure!(st == 200, "{st} {made}\n{}", srv.log());
    let id = s(&made, "/id");
    anyhow::ensure!(s(&made, "/owner").starts_with("user:") && made.get("version") == Some(&json!(1)), "{made}");
    for k in ["harness", "instructions", "tools", "mcp", "skills", "limits", "agentfile"] {
        anyhow::ensure!(made.get(k) == v1.get(k), "{k}: {made}");
    }
    let (st, dup) = call(&srv, &ada, "POST", "/api/agentdefs", Some(v1.clone()))?;
    anyhow::ensure!(st == 409, "a name taken: {st} {dup}");

    // Changed: new settings a new version; a rename alone none; a restore a new version again.
    let (st, v2) = call(&srv, &ada, "PATCH", &format!("/api/agentdefs/{id}"), Some(json!({ "instructions": "Review it twice.", "skills": [] })))?;
    anyhow::ensure!(st == 200 && v2.get("version") == Some(&json!(2)) && s(&v2, "/instructions") == "Review it twice." && v2.get("tools") == v1.get("tools"), "{st} {v2}");
    let (st, renamed) = call(&srv, &ada, "PATCH", &format!("/api/agentdefs/{id}"), Some(json!({ "name": "critic" })))?;
    anyhow::ensure!(st == 200 && renamed.get("version") == Some(&json!(2)) && s(&renamed, "/name") == "critic", "a rename is no version: {st} {renamed}");
    let (st, v3) = call(&srv, &ada, "POST", &format!("/api/agentdefs/{id}/restore"), Some(json!({ "version": 1 })))?;
    anyhow::ensure!(st == 200 && v3.get("version") == Some(&json!(3)) && v3.get("instructions") == v1.get("instructions") && v3.get("skills") == v1.get("skills") && s(&v3, "/name") == "critic", "{st} {v3}");
    let (_, vs) = call(&srv, &ada, "GET", &format!("/api/agentdefs/{id}/versions"), None)?;
    let listed: Vec<Value> = vs.get("versions").and_then(Value::as_array).into_iter().flatten().map(|v| v.get("version").cloned().unwrap_or_default()).collect();
    anyhow::ensure!(listed == [json!(3), json!(2), json!(1)], "{vs}");
    let (_, mine) = call(&srv, &ada, "GET", "/api/agentdefs", None)?;
    let names: Vec<String> = mine.get("agents").and_then(Value::as_array).into_iter().flatten().map(|a| s(a, "/name")).collect();
    anyhow::ensure!(names == ["critic"] && mine.get("reaches_all") == Some(&json!(false)), "{mine}");

    // The same operations as her MCP tools and in code mode.
    let (failed, said) = mcp(&srv, &ada, "agentdefs_get", json!({ "id": id }))?;
    anyhow::ensure!(!failed && serde_json::from_str::<Value>(&said).ok().and_then(|v| v.get("version").cloned()) == Some(json!(3)), "her tool: {said}");
    let (failed, said) = mcp(&srv, &ada, "agentdefs_create", json!({ "name": "fixer", "harness": "codex" }))?;
    anyhow::ensure!(!failed && said.contains("\"version\":1"), "her tool keeps one: {said}");
    let (failed, said) = mcp(&srv, &ada, "code_run", json!({ "program": "return (await kiln.agentdefs.list({})).agents.map(a => a.name).sort().join(',')" }))?;
    anyhow::ensure!(!failed && said.contains("critic,fixer"), "her code mode: {said}");

    // Bo reaches none of Ada's: as no agent definition, on every surface; his list is his own.
    for (method, path, body) in [
        ("GET", format!("/api/agentdefs/{id}"), None),
        ("GET", format!("/api/agentdefs/{id}/versions"), None),
        ("PATCH", format!("/api/agentdefs/{id}"), Some(json!({ "instructions": "mine now" }))),
        ("POST", format!("/api/agentdefs/{id}/restore"), Some(json!({ "version": 1 }))),
        ("POST", format!("/api/agentdefs/{id}/run"), Some(json!({ "prompt": "go" }))),
        ("DELETE", format!("/api/agentdefs/{id}"), None),
    ] {
        let (st, said) = call(&srv, &bo, method, &path, body)?;
        anyhow::ensure!(st == 404 && said.to_string().contains("no such agent definition"), "bo's {method} {path}: {st} {said}");
    }
    let (st, said) = call(&srv, &bo, "POST", "/api/ops/agentdefs.get", Some(json!({ "id": id })))?;
    anyhow::ensure!(st == 404, "the operation: {st} {said}");
    let (failed, said) = mcp(&srv, &bo, "agentdefs_get", json!({ "id": id }))?;
    anyhow::ensure!(failed && said.contains("no such agent definition"), "his tool: {said}");
    let (failed, said) = mcp(&srv, &bo, "code_run", json!({ "program": "return await kiln.agentdefs.get({ id: input.id })", "input": { "id": id } }))?;
    anyhow::ensure!(failed && said.contains("no such agent definition"), "his code mode: {said}");
    let (_, theirs) = call(&srv, &bo, "GET", "/api/agentdefs", None)?;
    anyhow::ensure!(theirs.get("agents") == Some(&json!([])), "{theirs}");
    let (st, said) = call(&srv, &bo, "GET", "/api/agentdefs?all=true", None)?;
    anyhow::ensure!(st == 403, "everyone's: {st} {said}");
    // Nor may he name her Agentfile in one of his: as none.
    let (st, said) = call(&srv, &bo, "POST", "/api/agentdefs", Some(json!({ "name": "thief", "harness": "claude", "agentfile": agentfile })))?;
    anyhow::ensure!(st == 422 && said.to_string().contains("names no Agentfile of yours"), "{st} {said}");

    // An admin reaches everyone's.
    let (st, all) = call(&srv, &root, "GET", "/api/agentdefs?all=true", None)?;
    anyhow::ensure!(st == 200 && all.get("reaches_all") == Some(&json!(true)) && all.get("agents").and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| s(x, "/id") == id)), "{st} {all}");

    // Each change audited, as its route, saying what it did.
    let (_, audit) = call(&srv, &srv.admin, "GET", "/api/audit?limit=200", None)?;
    let said: Vec<String> = audit.as_array().into_iter().flatten().filter_map(|r| r.get("detail").and_then(Value::as_str)).filter(|d| d.starts_with("agentdef.")).map(|d| d.split(':').next().unwrap_or_default().to_owned()).collect();
    anyhow::ensure!(said.iter().filter(|d| *d == "agentdef.create").count() == 2 && said.contains(&"agentdef.update".to_owned()) && said.contains(&"agentdef.restore".to_owned()), "{audit}");

    // Deleted, with its versions.
    let (st, said) = call(&srv, &ada, "DELETE", &format!("/api/agentdefs/{id}"), None)?;
    anyhow::ensure!(st == 200 && said == json!({ "deleted": true }), "{st} {said}");
    let (st, _) = call(&srv, &ada, "GET", &format!("/api/agentdefs/{id}/versions"), None)?;
    anyhow::ensure!(st == 404, "{st}");
    Ok(())
}

/// A PATCH of an agent definition with `If-Match` (and the token's own headers): its status and its body read.
fn patch_if(s: &Server, token: &str, id: &str, if_match: &str, body: &Value) -> anyhow::Result<(u16, Value)> {
    let headers = format!("Authorization: Bearer {token}\r\nContent-Type: application/json\r\nIf-Match: {if_match}\r\n");
    let (st, bytes) = raw(s.port, "PATCH", &format!("/api/agentdefs/{id}"), &headers, &serde_json::to_vec(body)?)?;
    Ok((st, serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))))
}

/// The UI's save (kiln plan 01a1dd2e…, P5.2's page; docs/ui/principles.md 5: "saves are explicit and carry their base
/// version; a save in between shows as an inline diff, never a silent overwrite"): two saves made over the same
/// version, by REST's `If-Match` and its `base`, by MCP and by code mode: the first is kept, the second refused 409,
/// saying the version it is now, and over REST carrying that version whole (`current`: what the first save made), so
/// the page can show what changed in between; nothing of it is written. A save over the version it is now is kept.
/// Eight saves made over one version at once: exactly one is kept, every other refused with the version it made.
/// `If-Match` and `base` naming two versions, or `If-Match` naming no version, are refused. Taking the base check
/// out of `agentdefs::update` keeps the second save; taking the version check out of its write can keep more than
/// one of the eight (each read version 3 before any wrote).
#[test]
fn p5_2_two_saves_from_one_base_the_second_is_refused_409_with_the_current_version() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let srv = start_env(dir.path(), &Options::default(), false, &[])?;
    let ada = person(&srv, "ada", &["developer"])?;
    let (st, made) = call(&srv, &ada, "POST", "/api/agentdefs", Some(json!({ "name": "reviewer", "harness": "claude", "instructions": "as made" })))?;
    anyhow::ensure!(st == 200 && made.get("version") == Some(&json!(1)), "{st} {made}\n{}", srv.log());
    let id = s(&made, "/id");

    // Two saves over version 1: the first kept (version 2), the second refused with version 2 whole.
    let (st, first) = patch_if(&srv, &ada, &id, "\"1\"", &json!({ "instructions": "the first save" }))?;
    anyhow::ensure!(st == 200 && first.get("version") == Some(&json!(2)), "{st} {first}");
    let (st, second) = patch_if(&srv, &ada, &id, "\"1\"", &json!({ "instructions": "the second save", "skills": ["review"] }))?;
    anyhow::ensure!(st == 409 && second.get("base") == Some(&json!(1)), "{st} {second}");
    anyhow::ensure!(s(&second, "/error").contains("changed since version 1") && s(&second, "/error").contains("it's version 2 now"), "{second}");
    anyhow::ensure!(second.pointer("/current/version") == Some(&json!(2)) && s(&second, "/current/instructions") == "the first save" && s(&second, "/current/id") == id, "the current version whole: {second}");
    let (_, now) = call(&srv, &ada, "GET", &format!("/api/agentdefs/{id}"), None)?;
    anyhow::ensure!(now.get("version") == Some(&json!(2)) && s(&now, "/instructions") == "the first save" && now.get("skills") == Some(&json!([])), "nothing of the second written: {now}");

    // The same over `base` in the body, the operation over HTTP, MCP and code mode: each refused, saying version 2.
    let (st, said) = call(&srv, &ada, "PATCH", &format!("/api/agentdefs/{id}"), Some(json!({ "base": 1, "instructions": "over base" })))?;
    anyhow::ensure!(st == 409 && said.pointer("/current/version") == Some(&json!(2)), "`base`: {st} {said}");
    let (st, said) = call(&srv, &ada, "POST", "/api/ops/agentdefs.update", Some(json!({ "id": id, "base": 1, "instructions": "the operation" })))?;
    anyhow::ensure!(st == 409 && said.to_string().contains("it's version 2 now"), "the operation: {st} {said}");
    let (failed, said) = mcp(&srv, &ada, "agentdefs_update", json!({ "id": id, "base": 1, "instructions": "her tool" }))?;
    anyhow::ensure!(failed && said.contains("changed since version 1") && said.contains("it's version 2 now"), "her tool: {said}");
    let (failed, said) = mcp(&srv, &ada, "code_run", json!({ "program": "return await kiln.agentdefs.update({ id: input.id, base: 1, instructions: 'code mode' })", "input": { "id": id } }))?;
    anyhow::ensure!(failed && said.contains("it's version 2 now"), "code mode: {said}");
    let (_, now) = call(&srv, &ada, "GET", &format!("/api/agentdefs/{id}"), None)?;
    anyhow::ensure!(now.get("version") == Some(&json!(2)) && s(&now, "/instructions") == "the first save", "none of them written: {now}");

    // Over the version it is now: kept.
    let (st, third) = patch_if(&srv, &ada, &id, "\"2\"", &json!({ "instructions": "over version 2" }))?;
    anyhow::ensure!(st == 200 && third.get("version") == Some(&json!(3)), "{st} {third}");

    // Eight saves over version 3 at once: one kept, seven refused, each with the version the kept one made.
    let answers: Vec<anyhow::Result<(u16, Value)>> = std::thread::scope(|sc| {
        let (srv, ada, id) = (&srv, &ada, &id);
        let handles: Vec<_> = (0..8).map(|n| sc.spawn(move || patch_if(srv, ada, id, "\"3\"", &json!({ "instructions": format!("racer {n}") })))).collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("a racer's thread panicked")))).collect()
    });
    let answers = answers.into_iter().collect::<anyhow::Result<Vec<_>>>()?;
    let kept: Vec<&Value> = answers.iter().filter(|(st, _)| *st == 200).map(|(_, v)| v).collect();
    anyhow::ensure!(kept.len() == 1 && kept.first().and_then(|v| v.get("version")) == Some(&json!(4)), "exactly one kept: {answers:?}");
    anyhow::ensure!(answers.iter().filter(|(st, v)| *st == 409 && v.pointer("/current/version") == Some(&json!(4))).count() == 7, "every other refused with version 4: {answers:?}");
    let (_, now) = call(&srv, &ada, "GET", &format!("/api/agentdefs/{id}"), None)?;
    anyhow::ensure!(now.get("version") == Some(&json!(4)) && kept.first().and_then(|v| v.get("instructions")) == now.get("instructions"), "the kept one is what it is: {now}");

    // Two versions named, or none: refused, nothing written.
    let (st, said) = patch_if(&srv, &ada, &id, "\"4\"", &json!({ "base": 3, "instructions": "which?" }))?;
    anyhow::ensure!(st == 400 && said.to_string().contains("name two versions"), "{st} {said}");
    let (st, said) = patch_if(&srv, &ada, &id, "W/\"abc\"", &json!({ "instructions": "no version" }))?;
    anyhow::ensure!(st == 400 && said.to_string().contains("If-Match names the version"), "{st} {said}");
    let (_, versions) = call(&srv, &ada, "GET", &format!("/api/agentdefs/{id}/versions"), None)?;
    anyhow::ensure!(versions.get("versions").and_then(Value::as_array).map(Vec::len) == Some(4), "four versions, no more: {versions}");
    Ok(())
}

/// An operation over HTTP (`POST /api/ops/<name>`) with `token`: its status and its answer.
fn op(s: &Server, token: &str, name: &str, input: Value) -> anyhow::Result<(u16, Value)> {
    call(s, token, "POST", &format!("/api/ops/{name}"), Some(input))
}

/// Agent `id` as agents.get answers, once `holds` says it's as wanted (within two minutes).
fn agent_until(s: &Server, id: &str, what: &str, holds: impl Fn(&Value) -> bool) -> anyhow::Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (st, got) = op(s, &s.admin, "agents.get", json!({ "agent": id }))?;
        if st == 200 && holds(&got) {
            return Ok(got);
        }
        anyhow::ensure!(Instant::now() < deadline, "{id} never {what}: {st} {got}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// P5.2's page (kiln plan 01a1dd2e…: an agent's row holds its sessions, each with the version it started from): a
/// session kiln starts from a definition says which, and at which version, and the definition lists its sessions
/// with what their starts took. Here kiln's about card (`agents.about`, a UI agent), on a runner of this build whose
/// harness is the stand-in Claude Code: started from the person's own copy standing in for kiln's (`agentdefs.use`),
/// `agents.get` says her copy at version 2; her copy's sessions list it (its version, runner, state and engine), and
/// its start (on its machine's host, timed from its open to its harness running); a new version of her copy leaves it
/// behind (`current` 3). With kiln's own again, the next card says kiln's (`kiln:kiln-about`, version 0) and is listed
/// under kiln's, as hers alone. Another person reaches neither her copy's list nor her cards in kiln's. A session that
/// started from no definition says none. On REST, MCP and code mode. Taking the definition out of `uses::ask_for`
/// lists no session; taking `record` out of `lineage::start` lists none either.
#[test]
fn p5_2_a_session_says_the_definition_it_started_from_and_the_definition_lists_its_sessions() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let srv = common::start(dir.path(), &Options::default())?;
    let (runner, log) = agents::runner_with(dir.path(), srv.port, "box", None, Talk { lines: 2, every_ms: 50 }, &[])?;
    agents::connected(srv.port, &srv.admin, "box", &log, Duration::from_secs(120))?;
    agents::headroom_off(srv.port, &srv.admin, "box", Duration::from_secs(60))?;
    let bo = person(&srv, "bo", &["developer"])?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    let main = agents::open_session(srv.port, &srv.admin, "box", &work, Duration::from_secs(90))?;
    agent_until(&srv, &main, "started its harness", |g| g.pointer("/info/starts/0").is_some())?;

    // Her copy of kiln's about card, at version 2, standing in for kiln's for her.
    let (st, copy) = call(&srv, &srv.admin, "POST", "/api/agentdefs/kiln:kiln-about/copy", Some(json!({ "name": "my-about" })))?;
    anyhow::ensure!(st == 200, "{st} {copy}");
    let mine = s(&copy, "/id");
    let (st, v2) = call(&srv, &srv.admin, "PATCH", &format!("/api/agentdefs/{mine}"), Some(json!({ "base": 1, "instructions": "Answer in one sentence." })))?;
    anyhow::ensure!(st == 200 && v2.get("version") == Some(&json!(2)), "{st} {v2}");
    let (st, used) = call(&srv, &srv.admin, "POST", "/api/agentdefs/use", Some(json!({ "builtin": "kiln:kiln-about", "id": mine })))?;
    anyhow::ensure!(st == 200, "{st} {used}");
    let ask = json!({ "agent": main, "path": "src/lib.rs", "line": 3, "code": "fn main() {}", "selection": "main" });

    // Her card: it says her copy at version 2, and her copy lists it with its start.
    let (st, asked) = op(&srv, &srv.admin, "agents.about", ask.clone())?;
    let card = asked.pointer("/agents/0/id").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no card started: {st} {asked}"))?;
    let got = agent_until(&srv, &card, "started its harness", |g| g.pointer("/info/starts/0").is_some())?;
    anyhow::ensure!(got.get("definition") == Some(&json!({ "id": mine, "version": 2, "name": "my-about", "current": 2 })), "her card's definition: {got}");
    let (st, listed) = call(&srv, &srv.admin, "GET", &format!("/api/agentdefs/{mine}/sessions"), None)?;
    anyhow::ensure!(st == 200 && listed.get("sessions").and_then(Value::as_array).map(Vec::len) == Some(1), "{st} {listed}");
    anyhow::ensure!(
        s(&listed, "/sessions/0/id") == card && listed.pointer("/sessions/0/version") == Some(&json!(2)) && s(&listed, "/sessions/0/runner") == machine("box") && s(&listed, "/sessions/0/engine") == "host",
        "her card, as listed: {listed}"
    );
    anyhow::ensure!(!s(&listed, "/sessions/0/state").is_empty() && s(&listed, "/sessions/0/state") != "gone" && s(&listed, "/sessions/0/agent/id") == card, "{listed}");
    let host = listed.pointer("/starts/by_kind/host").cloned().unwrap_or_default();
    anyhow::ensure!(listed.pointer("/starts/n").and_then(Value::as_u64) >= Some(1) && host.get("timed") == Some(&json!(1)) && host.get("p50_ms").is_some_and(Value::is_u64), "its start: {listed}");
    anyhow::ensure!(listed.pointer("/starts/warm") == Some(&Value::Null) && listed.pointer("/starts/by_kind/microvm").is_none(), "no warm pools, no microVM starts: {listed}");

    // A new version of her copy: her card is behind it.
    let (st, v3) = call(&srv, &srv.admin, "PATCH", &format!("/api/agentdefs/{mine}"), Some(json!({ "base": 2, "instructions": "Answer in two sentences." })))?;
    anyhow::ensure!(st == 200 && v3.get("version") == Some(&json!(3)), "{st} {v3}");
    let (_, got) = op(&srv, &srv.admin, "agents.get", json!({ "agent": card }))?;
    anyhow::ensure!(got.pointer("/definition/version") == Some(&json!(2)) && got.pointer("/definition/current") == Some(&json!(3)), "behind its definition: {got}");

    // The same over MCP and code mode.
    let (failed, said) = mcp(&srv, &srv.admin, "agentdefs_sessions", json!({ "id": mine }))?;
    anyhow::ensure!(!failed && said.contains(&card), "the tool: {said}");
    let (failed, said) = mcp(&srv, &srv.admin, "code_run", json!({ "program": "const l = await kiln.agentdefs.sessions({ id: input.id }); return `${l.sessions[0].version}/${l.starts.n >= 1}`", "input": { "id": mine } }))?;
    anyhow::ensure!(!failed && said.contains("2/true"), "code mode: {said}");

    // Another person reaches none of it.
    let (st, said) = call(&srv, &bo, "GET", &format!("/api/agentdefs/{mine}/sessions"), None)?;
    anyhow::ensure!(st == 404 && said.to_string().contains("no such agent definition"), "{st} {said}");
    let (st, said) = call(&srv, &bo, "GET", "/api/agentdefs/kiln:nothing/sessions", None)?;
    anyhow::ensure!(st == 404, "{st} {said}");

    // Kiln's own again: the next card says kiln's, listed under kiln's as hers alone.
    let (st, used) = call(&srv, &srv.admin, "POST", "/api/agentdefs/use", Some(json!({ "builtin": "kiln:kiln-about", "id": null })))?;
    anyhow::ensure!(st == 200, "{st} {used}");
    let (st, asked) = op(&srv, &srv.admin, "agents.about", ask)?;
    let kilns = asked.pointer("/agents/0/id").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no card started: {st} {asked}"))?;
    let got = agent_until(&srv, &kilns, "said its definition", |g| g.get("definition").is_some_and(|d| !d.is_null()))?;
    anyhow::ensure!(got.get("definition") == Some(&json!({ "id": "kiln:kiln-about", "version": 0, "name": "kiln-about", "current": 0 })), "kiln's card: {got}");
    let (st, listed) = call(&srv, &srv.admin, "GET", "/api/agentdefs/kiln:kiln-about/sessions", None)?;
    anyhow::ensure!(st == 200 && s(&listed, "/sessions/0/id") == kilns && listed.pointer("/sessions/0/version") == Some(&json!(0)), "{st} {listed}");
    let (st, his) = call(&srv, &bo, "GET", "/api/agentdefs/kiln:kiln-about/sessions", None)?;
    anyhow::ensure!(st == 200 && his.get("sessions") == Some(&json!([])), "his of kiln's: {st} {his}");
    let (_, listed) = call(&srv, &srv.admin, "GET", &format!("/api/agentdefs/{mine}/sessions"), None)?;
    anyhow::ensure!(listed.get("sessions").and_then(Value::as_array).map(Vec::len) == Some(1), "her copy's still its one: {listed}");

    // A session that started from no definition says none.
    let (_, got) = op(&srv, &srv.admin, "agents.get", json!({ "agent": main }))?;
    anyhow::ensure!(got.get("definition") == Some(&Value::Null), "{got}");

    // Deleted, her copy's sessions no longer say it.
    let (st, _) = call(&srv, &srv.admin, "DELETE", &format!("/api/agentdefs/{mine}"), None)?;
    anyhow::ensure!(st == 200, "{st}");
    let (_, got) = op(&srv, &srv.admin, "agents.get", json!({ "agent": card }))?;
    anyhow::ensure!(got.get("definition") == Some(&Value::Null), "{got}");
    drop(runner);
    Ok(())
}

/// P5.1's test "an agent naming an unknown harness is refused": kept nowhere, on REST, MCP and code
/// mode, every reason said; checking it keeps nothing and says the same; changing a kept one to it is
/// refused, the kept one as it was. A harness of kiln's ten is kept. Taking the harness rule out of
/// `agentdefs::check` keeps it.
#[test]
fn p5_1_an_agent_naming_an_unknown_harness_is_refused() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let srv = start_env(dir.path(), &Options::default(), false, &[])?;
    let ada = person(&srv, "ada", &["developer"])?;
    let unknown = "`aider` isn't one of kiln's harnesses: claude, codex, devin, gemini, opencode, goose, amp, antigravity, cursor, pi";

    let (st, said) = call(&srv, &ada, "POST", "/api/agentdefs", Some(json!({ "name": "helper", "harness": "aider", "limits": { "tokens": 0 } })))?;
    anyhow::ensure!(st == 422 && said.to_string().contains(&format!("kiln can't run this agent: {unknown}")), "{st} {said}\n{}", srv.log());
    let (failed, said) = mcp(&srv, &ada, "agentdefs_create", json!({ "name": "helper", "harness": "aider" }))?;
    anyhow::ensure!(failed && said.contains(unknown), "her tool: {said}");
    let (failed, said) = mcp(&srv, &ada, "code_run", json!({ "program": "return await kiln.agentdefs.create({ name: 'helper', harness: 'aider' })" }))?;
    anyhow::ensure!(failed && said.contains(unknown), "code mode: {said}");
    let (_, mine) = call(&srv, &ada, "GET", "/api/agentdefs", None)?;
    anyhow::ensure!(mine.get("agents") == Some(&json!([])), "nothing kept: {mine}");

    let (st, checked) = call(&srv, &ada, "POST", "/api/agentdefs/check", Some(json!({ "harness": "aider" })))?;
    anyhow::ensure!(st == 200 && checked == json!({ "ok": false, "problems": [unknown] }), "{st} {checked}");
    let (st, checked) = call(&srv, &ada, "POST", "/api/agentdefs/check", Some(json!({ "harness": "pi", "limits": { "seconds": 0 }, "mcp": [{ "name": "x" }] })))?;
    let problems: Vec<String> = checked.get("problems").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    anyhow::ensure!(
        st == 200 && checked.get("ok") == Some(&json!(false)) && problems.iter().any(|p| p.contains("`url` or started by its `command`")) && problems.iter().any(|p| p.contains("a limit of 0 seconds")),
        "every reason: {st} {checked}"
    );

    let (st, made) = call(&srv, &ada, "POST", "/api/agentdefs", Some(json!({ "name": "helper", "harness": "goose" })))?;
    anyhow::ensure!(st == 200 && s(&made, "/harness") == "goose", "{st} {made}");
    let id = s(&made, "/id");
    let (st, said) = call(&srv, &ada, "PATCH", &format!("/api/agentdefs/{id}"), Some(json!({ "harness": "aider" })))?;
    anyhow::ensure!(st == 422 && said.to_string().contains(unknown), "{st} {said}");
    let (_, now) = call(&srv, &ada, "GET", &format!("/api/agentdefs/{id}"), None)?;
    anyhow::ensure!(s(&now, "/harness") == "goose" && now.get("version") == Some(&json!(1)), "as it was: {now}");
    Ok(())
}

/// P5.1, kiln's own (the person, 2026-11-14: "One per ci skill and one for each harness we support.
/// Also don't forget the ask/about popup agents"): agentdefs.list shows them beside a person's own,
/// read only: one per CI skill kiln ships, one per harness in kiln's Agentfile for it, and the agents
/// kiln starts from its UI (the about card, the same for code no agent made, an attachment's agent, a
/// review agent, the tool fixer), each of them settings kiln runs (agentdefs.check passes it). One is
/// copied as yours and edited, on REST and MCP; kiln's own can't be changed, restored or deleted.
/// Only a replaceable one of kiln's takes a stand-in, and only one of your own (P5.1b; the stand-in
/// at work is `agentdefs::tests`'). Taking the read-only rule out of `agentdefs::record` answers 404,
/// not 403.
#[test]
fn p5_1_kilns_own_agent_definitions_are_listed_copied_and_read_only() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let srv = start_env(dir.path(), &Options::default(), false, &[])?;
    let ada = person(&srv, "ada", &["developer"])?;
    let harnesses = ["claude", "codex", "devin", "gemini", "opencode", "goose", "amp", "antigravity", "cursor", "pi"];
    let mut want: Vec<String> = ["author", "diagnose", "runners", "review", "migrate", "secrets", "performance"].iter().map(|s| format!("kiln-ci-{s}")).collect();
    want.extend(harnesses.iter().map(|h| format!("harness-{h}")));
    want.extend(["kiln-about", "kiln-about-code", "kiln-attach", "kiln-review", "kiln-toolfix"].map(str::to_owned));

    let (st, listed) = call(&srv, &ada, "GET", "/api/agentdefs", None)?;
    let builtin: Vec<Value> = listed.get("builtin").and_then(Value::as_array).cloned().unwrap_or_default();
    let names: Vec<String> = builtin.iter().map(|b| s(b, "/name")).collect();
    anyhow::ensure!(st == 200 && names == want && listed.get("agents") == Some(&json!([])), "{st} {names:?}\n{listed}");
    for b in &builtin {
        anyhow::ensure!(s(b, "/id") == format!("kiln:{}", s(b, "/name")), "{b}");
        let mut settings = json!({});
        for k in ["harness", "model", "instructions", "tools", "mcp", "skills", "limits", "agentfile"] {
            if let (Some(o), Some(v)) = (settings.as_object_mut(), b.get(k)) {
                o.insert(k.into(), v.clone());
            }
        }
        let (st, checked) = call(&srv, &ada, "POST", "/api/agentdefs/check", Some(settings))?;
        anyhow::ensure!(st == 200 && checked.get("ok") == Some(&json!(true)), "{}: {st} {checked}", s(b, "/name"));
    }
    let ci = builtin.iter().find(|b| s(b, "/name") == "kiln-ci-author").cloned().unwrap_or_default();
    anyhow::ensure!(ci.get("skills") == Some(&json!(["kiln-ci-author"])) && s(&ci, "/instructions").contains("skill://kiln-ci-author/SKILL.md"), "{ci}");
    for h in harnesses {
        let b = builtin.iter().find(|b| s(b, "/name") == format!("harness-{h}")).cloned().unwrap_or_default();
        anyhow::ensure!(s(&b, "/harness") == h && s(&b, "/agentfile") == format!("harness-{h}"), "{b}");
    }
    let replaceable: Vec<String> = builtin.iter().filter(|b| b.get("replaceable") == Some(&json!(true))).map(|b| s(b, "/name")).collect();
    anyhow::ensure!(replaceable == ["kiln-about", "kiln-about-code", "kiln-attach", "kiln-review", "kiln-toolfix"] && listed.get("uses") == Some(&json!({})), "{replaceable:?} {listed}");
    let review = builtin.iter().find(|b| s(b, "/name") == "kiln-review").cloned().unwrap_or_default();
    // A UI agent's brief is kiln's, told after the instructions of whichever definition starts: kiln's own have none.
    anyhow::ensure!(s(&review, "/brief").contains("ci.diffs.review gives your verdict") && s(&review, "/brief").contains("incidents.report") && s(&review, "/instructions").is_empty(), "{review}");
    let (failed, said) = mcp(&srv, &ada, "agentdefs_get", json!({ "id": "kiln:kiln-about" }))?;
    anyhow::ensure!(!failed && said.contains("## What it is") && said.contains("<selection>"), "the about card's brief: {said}");

    // Kiln's own are read only.
    for (method, path, body) in [
        ("PATCH", "/api/agentdefs/kiln:kiln-about", Some(json!({ "instructions": "mine" }))),
        ("POST", "/api/agentdefs/kiln:kiln-about/restore", Some(json!({ "version": 1 }))),
        ("DELETE", "/api/agentdefs/kiln:kiln-about", None),
    ] {
        let (st, said) = call(&srv, &ada, method, path, body)?;
        anyhow::ensure!(st == 403 && said.to_string().contains("kiln:kiln-about is kiln's own agent definition: copy it as yours"), "{method} {path}: {st} {said}");
    }

    // Copied as hers: her own, version 1, its settings kiln's; then hers to change.
    let (st, copied) = call(&srv, &ada, "POST", "/api/agentdefs/kiln:kiln-ci-author/copy", Some(json!({})))?;
    anyhow::ensure!(st == 200 && s(&copied, "/owner").starts_with("user:") && s(&copied, "/name") == "kiln-ci-author" && copied.get("version") == Some(&json!(1)), "{st} {copied}");
    anyhow::ensure!(copied.get("instructions") == ci.get("instructions") && copied.get("skills") == ci.get("skills"), "{copied}");
    let id = s(&copied, "/id");
    let (st, edited) = call(&srv, &ada, "PATCH", &format!("/api/agentdefs/{id}"), Some(json!({ "instructions": "Author it my way." })))?;
    anyhow::ensure!(st == 200 && edited.get("version") == Some(&json!(2)) && s(&edited, "/instructions") == "Author it my way.", "{st} {edited}");
    let (failed, said) = mcp(&srv, &ada, "agentdefs_copy", json!({ "id": "kiln:harness-codex", "name": "my-codex" }))?;
    anyhow::ensure!(!failed && said.contains("\"name\":\"my-codex\"") && said.contains("\"agentfile\":\"harness-codex\""), "her tool: {said}");
    // Her copy may stand in for kiln's about card for her; not for one of kiln's that isn't a UI agent
    // it starts for her, and not another person's copy.
    let bo = person(&srv, "bo", &["developer"])?;
    let (_, his) = call(&srv, &bo, "POST", "/api/agentdefs/kiln:kiln-about/copy", Some(json!({})))?;
    let (st, said) = call(&srv, &ada, "POST", "/api/agentdefs/use", Some(json!({ "builtin": "kiln:kiln-about", "id": s(&his, "/id") })))?;
    anyhow::ensure!(st == 404, "his copy: {st} {said}");
    let (st, said) = call(&srv, &ada, "POST", "/api/agentdefs/use", Some(json!({ "builtin": "kiln:kiln-ci-author", "id": id })))?;
    anyhow::ensure!(st == 400 && said.to_string().contains("kiln:kiln-about, kiln:kiln-about-code, kiln:kiln-attach, kiln:kiln-review, kiln:kiln-toolfix"), "{st} {said}");
    let (st, used) = call(&srv, &ada, "POST", "/api/agentdefs/use", Some(json!({ "builtin": "kiln:kiln-about", "id": id })))?;
    anyhow::ensure!(st == 200 && used == json!({ "uses": { "kiln:kiln-about": id } }), "{st} {used}");
    let (_, his_list) = call(&srv, &bo, "GET", "/api/agentdefs", None)?;
    anyhow::ensure!(his_list.get("uses") == Some(&json!({})), "only her use changes: {his_list}");
    let (_, after) = call(&srv, &ada, "GET", "/api/agentdefs", None)?;
    anyhow::ensure!(after.get("uses") == Some(&json!({ "kiln:kiln-about": id })), "{after}");
    let theirs: Vec<String> = after.get("builtin").and_then(Value::as_array).into_iter().flatten().map(|b| s(b, "/name")).collect();
    anyhow::ensure!(theirs == want && after.get("agents").and_then(Value::as_array).map(Vec::len) == Some(2), "kiln's own as they were: {after}");
    Ok(())
}

/// kiln plan 01a1dd2e8618733a87f2793aab1322a8 P5.6, slice S2: the Agents page's interrupt, controls and
/// restart are operations (agents.interrupt, agents.control, agents.restart, listed with every other),
/// and the bare command route takes none of them, saying which operation does. Taking the refusal out
/// of the command route answers those commands there again.
#[test]
fn p5_6_s2_the_agents_pages_controls_are_operations_and_the_command_route_takes_none() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let srv = start_env(dir.path(), &Options::default(), false, &[])?;
    for (c, op) in [(json!({"type": "interrupt"}), "interrupt"), (json!({"type": "control", "id": "rename", "value": "x"}), "control"), (json!({"type": "restart"}), "restart")] {
        let (st, said) = call(&srv, &srv.admin, "POST", "/api/harness/sessions/s-none/command", Some(c))?;
        anyhow::ensure!(st == 400 && said.to_string().contains(&format!("agents.{op}'s: POST /api/harness/agents/s-none/{op}")), "{op}: {st} {said}\n{}", srv.log());
    }
    let (st, ops) = call(&srv, &srv.admin, "GET", "/api/ops", None)?;
    let names: Vec<String> = ops.get("operations").and_then(Value::as_array).into_iter().flatten().map(|o| s(o, "/name")).collect();
    anyhow::ensure!(st == 200 && ["agents.control", "agents.restart", "agents.interrupt", "agents.remove"].iter().all(|n| names.iter().any(|m| m == n)), "{names:?}");
    let (st, said) = call(&srv, &srv.admin, "POST", "/api/harness/agents/s-none/control", Some(json!({ "control": "rename", "value": "x" })))?;
    anyhow::ensure!(st == 404, "no such agent: {st} {said}");
    Ok(())
}
