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

use common::{Options, Server, http, start_env};
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
