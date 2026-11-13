//! Agentfiles kept in kiln (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 30), through each surface
//! people and agents reach them by: the routes, the operations over HTTP, MCP and code mode. Two
//! developers each keep their own and reach no one else's; an admin reaches both; a text the server's
//! Shards refuses isn't kept, in Shards' words; a project's own are listed read only. The server's
//! `shards` is a stand-in that answers as shards 0.7.0 answers its check (`crate::agentfiles::check`'s
//! recorded answers): a `FORM` line refused at its line, anything else passed.
//!
//! `cargo test -p kiln --test agentfiles`

#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, Server, http, start_env};
use serde_json::{Value, json};

/// Answers as `shards build --call=check,format=json --target kiln:static-check` does.
const SHARDS: &str = r#"#!/bin/sh
f=""
while [ $# -gt 0 ]; do
  if [ "$1" = "-f" ]; then f="$2"; fi
  shift
done
line=$(grep -n '^FORM' "$f" | head -n 1 | cut -d: -f1)
if [ -n "$line" ]; then
  printf '{"warnings":null,"sources":[{"filename":"Agentfile","language":"Agentfile"}],"buildError":{"message":"dockerfile parse error on line %s: unknown instruction: FORM (did you mean FROM?)","location":{"ranges":[{"start":{"line":%s},"end":{"line":%s}}]}}}\n' "$line" "$line" "$line"
  echo "ERROR: dockerfile parse error on line $line: unknown instruction: FORM (did you mean FROM?)" >&2
else
  printf '{"warnings":null,"sources":[{"filename":"Agentfile","language":"Agentfile"}],"buildError":{"message":"target stage \\"kiln:static-check\\" could not be found","location":{}}}\n'
  echo 'ERROR: target stage "kiln:static-check" could not be found' >&2
fi
exit 1
"#;

const CLAUDE: &str = "FROM ubuntu:24.04\nHARNESS claude FROM ./harness\nAGENT main FROM ./agent\nATTACH main FOR claude\nCMD [\"claude\"]\n";
const CLAUDE_2: &str = "FROM rust:1.91\nHARNESS claude FROM ./harness\nAGENT main FROM ./agent\nATTACH main FOR claude\nCMD [\"claude\"]\n";

fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    anyhow::ensure!(o.status.success(), "git {args:?}: {out}");
    Ok(out)
}

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

#[test]
fn step30_agentfiles_on_every_surface() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin)?;
    std::fs::write(bin.join("shards"), SHARDS)?;
    std::fs::set_permissions(bin.join("shards"), std::fs::Permissions::from_mode(0o755))?;
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let srv = start_env(dir.path(), &Options::default(), false, &[("PATH", &path)])?;
    let ada = person(&srv, "ada", &["developer"])?;
    let bo = person(&srv, "bo", &["developer"])?;
    let root = person(&srv, "root", &["admin"])?;

    // Kept, version 1, Shards' check with it.
    let (st, made) = call(&srv, &ada, "POST", "/api/agentfiles", Some(json!({ "name": "claude", "text": CLAUDE })))?;
    anyhow::ensure!(st == 200, "{st} {made}\n{}", srv.log());
    let id = s(&made, "/agentfile/id");
    anyhow::ensure!(s(&made, "/agentfile/owner").starts_with("user:") && made.pointer("/agentfile/version") == Some(&json!(1)), "{made}");
    anyhow::ensure!(made.pointer("/check") == Some(&json!({ "checked": true, "diagnostics": [] })), "{made}");
    // Refused in Shards' words: not kept.
    let (st, refused) = call(&srv, &ada, "POST", "/api/agentfiles", Some(json!({ "name": "broken", "text": "FROM ubuntu:24.04\nFORM ubuntu\n" })))?;
    anyhow::ensure!(st == 422 && refused.to_string().contains("Shards refuses this Agentfile: dockerfile parse error on line 2: unknown instruction: FORM (did you mean FROM?)"), "{st} {refused}");
    let (st, checked) = call(&srv, &ada, "POST", "/api/agentfiles/check", Some(json!({ "text": "FROM ubuntu:24.04\nFORM ubuntu\n" })))?;
    anyhow::ensure!(st == 200 && s(&checked, "/diagnostics/0/severity") == "error" && checked.pointer("/diagnostics/0/range/start/line") == Some(&json!(2)), "{st} {checked}");
    let (st, dup) = call(&srv, &ada, "POST", "/api/agentfiles", Some(json!({ "name": "claude", "text": CLAUDE })))?;
    anyhow::ensure!(st == 409, "{st} {dup}");
    let (_, listed) = call(&srv, &ada, "GET", "/api/agentfiles", None)?;
    let names: Vec<String> = listed.get("agentfiles").and_then(Value::as_array).into_iter().flatten().map(|a| s(a, "/name")).collect();
    let builtin = serde_json::to_value(kiln::agentfiles::builtin::views())?;
    anyhow::ensure!(names == ["claude"] && listed.get("builtin") == Some(&builtin) && listed.get("reaches_all") == Some(&json!(false)), "{listed}");

    // Bo reaches none of Ada's: as no Agentfile, on every surface; his list is his own.
    for (method, path, body) in [
        ("GET", format!("/api/agentfiles/{id}"), None),
        ("GET", format!("/api/agentfiles/{id}/versions"), None),
        ("PATCH", format!("/api/agentfiles/{id}"), Some(json!({ "text": CLAUDE_2 }))),
        ("POST", format!("/api/agentfiles/{id}/restore"), Some(json!({ "version": 1 }))),
        ("DELETE", format!("/api/agentfiles/{id}"), None),
    ] {
        let (st, said) = call(&srv, &bo, method, &path, body)?;
        anyhow::ensure!(st == 404 && said.to_string().contains("no such Agentfile"), "bo's {method} {path}: {st} {said}");
    }
    let (st, said) = call(&srv, &bo, "POST", "/api/ops/agentfiles.get", Some(json!({ "id": id })))?;
    anyhow::ensure!(st == 404, "the operation: {st} {said}");
    let (failed, said) = mcp(&srv, &bo, "agentfiles_get", json!({ "id": id }))?;
    anyhow::ensure!(failed && said.contains("no such Agentfile"), "its tool: {said}");
    let (failed, said) = mcp(&srv, &bo, "code_run", json!({ "program": "return await kiln.agentfiles.get({ id: input.id })", "input": { "id": id } }))?;
    anyhow::ensure!(failed && said.contains("no such Agentfile"), "code mode: {said}");
    let (_, theirs) = call(&srv, &bo, "GET", "/api/agentfiles", None)?;
    anyhow::ensure!(theirs.get("agentfiles") == Some(&json!([])), "{theirs}");
    let (st, said) = call(&srv, &bo, "GET", "/api/agentfiles?all=true", None)?;
    anyhow::ensure!(st == 403, "everyone's: {st} {said}");

    // Ada changes hers: a new text a new version, a restore another; her tool and code mode read them.
    let (st, v2) = call(&srv, &ada, "PATCH", &format!("/api/agentfiles/{id}"), Some(json!({ "text": CLAUDE_2 })))?;
    anyhow::ensure!(st == 200 && v2.pointer("/agentfile/version") == Some(&json!(2)), "{st} {v2}");
    let (st, v3) = call(&srv, &ada, "POST", &format!("/api/agentfiles/{id}/restore"), Some(json!({ "version": 1 })))?;
    anyhow::ensure!(st == 200 && v3.pointer("/agentfile/version") == Some(&json!(3)) && s(&v3, "/agentfile/text") == CLAUDE, "{st} {v3}");
    let (_, vs) = call(&srv, &ada, "GET", &format!("/api/agentfiles/{id}/versions"), None)?;
    let listed: Vec<Value> = vs.get("versions").and_then(Value::as_array).into_iter().flatten().map(|v| v.get("version").cloned().unwrap_or_default()).collect();
    anyhow::ensure!(listed == [json!(3), json!(2), json!(1)], "{vs}");
    let (failed, said) = mcp(&srv, &ada, "agentfiles_get", json!({ "id": id }))?;
    anyhow::ensure!(!failed && serde_json::from_str::<Value>(&said).ok().and_then(|v| v.get("version").cloned()) == Some(json!(3)), "her tool: {said}");
    let (failed, said) = mcp(&srv, &ada, "code_run", json!({ "program": "return (await kiln.agentfiles.list({})).agentfiles.map(a => a.name)" }))?;
    anyhow::ensure!(!failed && said.contains("claude"), "her code mode: {said}");

    // An admin reaches everyone's.
    let (st, all) = call(&srv, &root, "GET", "/api/agentfiles?all=true", None)?;
    anyhow::ensure!(st == 200 && all.get("reaches_all") == Some(&json!(true)) && all.get("agentfiles").and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| s(x, "/id") == id)), "{st} {all}");

    // Each change audited once, as its route, saying what it did.
    let (_, audit) = call(&srv, &srv.admin, "GET", "/api/audit?limit=200", None)?;
    let said: Vec<String> = audit.as_array().into_iter().flatten().filter_map(|r| r.get("detail").and_then(Value::as_str)).filter(|d| d.starts_with("agentfile.")).map(|d| d.split(':').next().unwrap_or_default().to_owned()).collect();
    anyhow::ensure!(said.iter().filter(|d| *d == "agentfile.create").count() == 1 && said.contains(&"agentfile.update".to_owned()) && said.contains(&"agentfile.restore".to_owned()), "{audit}");

    // A project's own, read only, at its default branch.
    let (st, body) = http(srv.port, "POST", "/api/repos", &srv.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("work-tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln/images/claude"))?;
    std::fs::write(work.join(".kiln/images/claude/Agentfile"), CLAUDE)?;
    std::fs::write(work.join(".kiln/images/Agentfile"), CLAUDE)?;
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "images"])?;
    git(&home, &work, &["push", "-q", &format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", srv.admin, srv.port), "HEAD:main"])?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let project = loop {
        let (st, l) = call(&srv, &ada, "GET", "/api/agentfiles?repo=acme/app", None)?;
        if st == 200 && l.pointer("/project/agentfiles/0").is_some() {
            break l;
        }
        anyhow::ensure!(Instant::now() < deadline, "the project's Agentfiles never showed: {st} {l}");
        std::thread::sleep(Duration::from_millis(300));
    };
    anyhow::ensure!(
        project.pointer("/project/agentfiles") == Some(&json!([{ "name": "claude", "path": ".kiln/images/claude/Agentfile", "text": CLAUDE }])) && s(&project, "/project/repo") == "acme/app",
        "{project}"
    );
    let (st, said) = call(&srv, &ada, "GET", "/api/agentfiles?repo=acme/none", None)?;
    anyhow::ensure!(st == 404, "{st} {said}");

    // Deleted, with its versions.
    let (st, said) = call(&srv, &ada, "DELETE", &format!("/api/agentfiles/{id}"), None)?;
    anyhow::ensure!(st == 200 && said == json!({ "deleted": true }), "{st} {said}");
    let (st, _) = call(&srv, &ada, "GET", &format!("/api/agentfiles/{id}/versions"), None)?;
    anyhow::ensure!(st == 404, "{st}");
    Ok(())
}

/// kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 35: a start names the Agentfile an isolated agent's
/// microVM is made from, through every surface a person or an agent starts one by (the sessions route,
/// `agents.start` as a route and as an MCP tool): kiln's own for its harness, or one of its person's
/// own. Another person's is as none (404), before anything is placed for it; kiln's for another harness
/// and an Agentfile with `isolation` "none" are refused; `resume` takes up one conversation, so with
/// `count` past 1 it's refused. Taking out `image_for`'s owner rule passes Bo's starts; taking out the
/// sessions route's call passes his first; taking out the count rule passes the last.
#[test]
fn step35_a_start_names_only_an_agentfile_its_person_reaches() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin)?;
    std::fs::write(bin.join("shards"), SHARDS)?;
    std::fs::set_permissions(bin.join("shards"), std::fs::Permissions::from_mode(0o755))?;
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let srv = start_env(dir.path(), &Options::default(), false, &[("PATH", &path)])?;
    // Each may open agents on a runner (`runners.terminal`), which the sessions route asks first.
    let ada = person(&srv, "ada", &["developer", "runners.terminal"])?;
    let bo = person(&srv, "bo", &["developer", "runners.terminal"])?;
    let (st, made) = call(&srv, &ada, "POST", "/api/agentfiles", Some(json!({ "name": "claude-rust", "text": CLAUDE })))?;
    anyhow::ensure!(st == 200, "{st} {made}\n{}", srv.log());
    let hers = format!("agentfile:{}", s(&made, "/agentfile/id"));
    let session = |image: &str| json!({ "target": "agent:vmbox", "spec": { "kind": "claude", "isolation": { "kind": "microvm", "image": image } } });
    let refused_as_none = |said: &str| said.contains("names no Agentfile of yours");

    // Bo can't name Ada's, on any surface; nothing is started for him.
    let (st, said) = call(&srv, &bo, "POST", "/api/harness/sessions", Some(session(&hers)))?;
    anyhow::ensure!(st == 404 && refused_as_none(&said.to_string()), "the sessions route: {st} {said}");
    let (st, said) = call(&srv, &bo, "POST", "/api/harness/agents", Some(json!({ "kind": "claude", "image": hers })))?;
    anyhow::ensure!(st == 404 && refused_as_none(&said.to_string()), "agents.start's route: {st} {said}");
    let (failed, said) = mcp(&srv, &bo, "agents_start", json!({ "kind": "claude", "image": hers }))?;
    anyhow::ensure!(failed && refused_as_none(&said), "agents_start: {said}");
    let (_, listed) = call(&srv, &srv.admin, "GET", "/api/harness/sessions", None)?;
    anyhow::ensure!(listed == json!([]), "nothing started: {listed}");

    // Ada's own passes the check (what refuses her next is where it would run: no machine here starts
    // microVMs), on every surface.
    let (st, said) = call(&srv, &ada, "POST", "/api/harness/sessions", Some(session(&hers)))?;
    anyhow::ensure!(!refused_as_none(&said.to_string()), "the sessions route, hers: {st} {said}");
    let (failed, said) = mcp(&srv, &ada, "agents_start", json!({ "kind": "claude", "image": hers }))?;
    anyhow::ensure!(failed && !refused_as_none(&said) && said.contains("microVM"), "agents_start, hers: {said}");

    // kiln's own for another harness, an Agentfile as a process, and a conversation taken up twice.
    let (st, said) = call(&srv, &ada, "POST", "/api/harness/sessions", Some(session("harness-codex")))?;
    anyhow::ensure!(st == 400 && said.to_string().contains("harness-codex is kiln's Agentfile for codex, not Claude Code"), "{st} {said}");
    let (failed, said) = mcp(&srv, &ada, "agents_start", json!({ "kind": "claude", "image": "harness-claude", "isolation": "none" }))?;
    anyhow::ensure!(failed && said.contains("`image` can't go with `isolation` \"none\""), "{said}");
    let (failed, said) = mcp(&srv, &ada, "agents_start", json!({ "kind": "claude", "resume": "c-1", "count": 2 }))?;
    anyhow::ensure!(failed && said.contains("one conversation is taken up by one agent"), "{said}");
    Ok(())
}
