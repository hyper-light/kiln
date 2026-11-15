//! A person's own secrets (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, decision 6; P4.7), through the
//! surfaces people and agents reach them by: the routes (`/api/me/secrets`), the operations over HTTP,
//! kiln's MCP tools and code mode. Each person sets, lists and deletes their own and sees no one
//! else's, the server's owner included; a build of a person's own Agentfile takes their secret by
//! reference (`LABEL kiln.secret.<ID>=user:<NAME>`), and another person's start of an Agentfile
//! naming the same name is refused before anything is placed; no value is in any answer, the server's
//! log, the audit trail or an Agentfile's text. What the runner's build is given is
//! `sandbox::image`'s `p4_7_*` test; a real build on a microVM runner is ROOT's.
//!
//! `cargo test -p kiln --test user_secrets`

#![cfg(unix)]

mod common;

use common::{Options, Server, http, start_env};
use serde_json::{Value, json};

const VALUE: &str = "ghp_p47_ada_only_0123456789";
const OTHER: &str = "npm_p47_ada_only_9876543210";

/// A person, `name`, with `grants`, and a token of theirs that does all they may.
fn person(s: &Server, name: &str, grants: &[&str]) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": grants })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}'s token: {st} {body}");
    serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))
}

/// Every answer the test read, to hold each to carrying no value.
#[derive(Default)]
struct Said(Vec<String>);

impl Said {
    fn call(&mut self, s: &Server, token: &str, method: &str, path: &str, body: Option<Value>) -> anyhow::Result<(u16, Value)> {
        let (st, text) = http(s.port, method, path, token, body.as_ref())?;
        self.0.push(text.clone());
        Ok((st, serde_json::from_str(&text).unwrap_or(Value::String(text))))
    }

    /// One MCP tool call with `token`: whether it failed, and what it said.
    fn mcp(&mut self, s: &Server, token: &str, tool: &str, arguments: Value) -> anyhow::Result<(bool, String)> {
        let (st, body) = http(s.port, "POST", "/api/mcp", token, Some(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": tool, "arguments": arguments } })))?;
        self.0.push(body.clone());
        anyhow::ensure!(st == 200, "{tool}: {st} {body}");
        let v: Value = serde_json::from_str(&body)?;
        Ok((v.pointer("/result/isError").and_then(Value::as_bool).unwrap_or(true), v.pointer("/result/content/0/text").and_then(Value::as_str).unwrap_or_default().to_owned()))
    }

    /// No answer, nor the server's log, carries a value.
    fn none_carry(&self, s: &Server, values: &[&str]) -> anyhow::Result<()> {
        let log = s.log();
        for v in values {
            anyhow::ensure!(!log.contains(v), "the server's log carries a value");
            if let Some(a) = self.0.iter().find(|a| a.contains(v)) {
                anyhow::bail!("an answer carries a value: {a}");
            }
        }
        Ok(())
    }
}

fn names(listed: &Value) -> Vec<String> {
    listed.get("secrets").and_then(Value::as_array).map(|a| a.iter().filter_map(|s| s.get("name").and_then(Value::as_str).map(str::to_owned)).collect()).unwrap_or_default()
}

/// P4.7 AC1, AC3 and AC4: Ada sets her own secrets (REST, MCP), lists them (REST, an operation over
/// HTTP, MCP, code mode) and deletes one; Bo, an admin and the server's owner see none of them, by any
/// surface, and `/api/ci/secrets` names no person's scope for anyone; no answer, log or audit row
/// carries a value.
#[test]
fn p4_7_a_persons_secrets_are_theirs_alone_on_every_surface() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let srv = start_env(dir.path(), &Options::default(), false, &[])?;
    let ada = person(&srv, "ada", &["developer"])?;
    let bo = person(&srv, "bo", &["developer"])?;
    let root = person(&srv, "root", &["admin"])?;
    let mut said = Said::default();

    // Set (REST and MCP), and listed by name, never by value.
    let (st, set) = said.call(&srv, &ada, "PUT", "/api/me/secrets/gh_token", Some(json!({ "value": VALUE })))?;
    anyhow::ensure!(st == 200 && set == json!({ "set": true, "name": "GH_TOKEN" }), "{st} {set}\n{}", srv.log());
    let (failed, out) = said.mcp(&srv, &ada, "secrets_mine_set", json!({ "name": "NPM_TOKEN", "value": OTHER }))?;
    anyhow::ensure!(!failed, "secrets_mine_set: {out}");
    let (st, listed) = said.call(&srv, &ada, "GET", "/api/me/secrets", None)?;
    anyhow::ensure!(st == 200 && names(&listed) == ["GH_TOKEN", "NPM_TOKEN"], "{st} {listed}");
    anyhow::ensure!(listed.pointer("/secrets/0/size") == Some(&json!(VALUE.len())) && listed.pointer("/secrets/0/value").is_none(), "{listed}");
    let (st, op) = said.call(&srv, &ada, "POST", "/api/ops/secrets.mine.list", Some(json!({})))?;
    anyhow::ensure!(st == 200 && names(&op) == ["GH_TOKEN", "NPM_TOKEN"], "the operation over HTTP: {st} {op}");
    let (failed, out) = said.mcp(&srv, &ada, "secrets_mine_list", json!({}))?;
    anyhow::ensure!(!failed && out.contains("GH_TOKEN") && out.contains("NPM_TOKEN"), "secrets_mine_list: {out}");
    let (failed, out) = said.mcp(&srv, &ada, "code_run", json!({ "program": "return (await kiln.secrets.mine.list({})).secrets.map(s => s.name)" }))?;
    anyhow::ensure!(!failed && out.contains("GH_TOKEN") && out.contains("NPM_TOKEN"), "code mode: {out}");

    // No one else sees them: Bo, an admin, the server's owner, on any surface.
    for (who, token) in [("bo", &bo), ("an admin", &root), ("the owner", &srv.admin)] {
        let (st, listed) = said.call(&srv, token, "GET", "/api/me/secrets", None)?;
        anyhow::ensure!(st == 200 && names(&listed).is_empty(), "{who}: {st} {listed}");
        let (failed, out) = said.mcp(&srv, token, "secrets_mine_list", json!({}))?;
        anyhow::ensure!(!failed && !out.contains("GH_TOKEN"), "{who}, MCP: {out}");
        let (st, gone) = said.call(&srv, token, "DELETE", "/api/me/secrets/GH_TOKEN", None)?;
        anyhow::ensure!(st == 404, "{who} deletes none of hers: {st} {gone}");
        for scope in ["user:ada", "user:owner", "user"] {
            let (st, refused) = said.call(&srv, token, "GET", &format!("/api/ci/secrets?scope={scope}"), None)?;
            anyhow::ensure!(st == 400 || st == 403, "{who}, scope {scope}: {st} {refused}");
            anyhow::ensure!(!refused.to_string().contains("GH_TOKEN"), "{who}, scope {scope}: {refused}");
        }
    }
    let (st, refused) = said.call(&srv, &srv.admin, "PUT", "/api/ci/secrets/GH_TOKEN?scope=user:ada", Some(json!({ "value": "x" })))?;
    anyhow::ensure!(st == 400 && refused.to_string().contains("theirs alone"), "the owner sets none of hers: {st} {refused}");
    // Bo's own of the same name is his, beside hers.
    let (st, _) = said.call(&srv, &bo, "PUT", "/api/me/secrets/GH_TOKEN", Some(json!({ "value": "bo's own" })))?;
    anyhow::ensure!(st == 200, "{st}");
    let (_, listed) = said.call(&srv, &ada, "GET", "/api/me/secrets", None)?;
    anyhow::ensure!(listed.pointer("/secrets/0/size") == Some(&json!(VALUE.len())), "hers is still hers: {listed}");

    // Deleted (and updated) by her.
    let (st, _) = said.call(&srv, &ada, "DELETE", "/api/me/secrets/npm_token", None)?;
    anyhow::ensure!(st == 200, "{st}");
    let (st, _) = said.call(&srv, &ada, "PUT", "/api/me/secrets/GH_TOKEN", Some(json!({ "value": format!("{VALUE}-2") })))?;
    anyhow::ensure!(st == 200, "{st}");
    let (_, listed) = said.call(&srv, &ada, "GET", "/api/me/secrets", None)?;
    anyhow::ensure!(names(&listed) == ["GH_TOKEN"] && listed.pointer("/secrets/0/size") == Some(&json!(VALUE.len() + 2)), "{listed}");

    // Every change audited, no value in its rows.
    let (st, audit) = said.call(&srv, &srv.admin, "GET", "/api/audit?q=me/secrets", None)?;
    anyhow::ensure!(st == 200 && audit.to_string().contains("/api/me/secrets/"), "{st} {audit}");
    said.none_carry(&srv, &[VALUE, OTHER])
}

const CLAUDE_PRIVATE: &str = "FROM ubuntu:24.04\nLABEL kiln.secret.GIT_AUTH_TOKEN=user:GH_TOKEN\nHARNESS claude FROM ./harness\nAGENT main FROM ./agent\nATTACH main FOR claude\nCMD [\"claude\"]\n";

fn agentfile(said: &mut Said, s: &Server, token: &str, name: &str, text: &str) -> anyhow::Result<String> {
    let (st, made) = said.call(s, token, "POST", "/api/agentfiles", Some(json!({ "name": name, "text": text })))?;
    anyhow::ensure!(st == 200, "{name}: {st} {made}\n{}", s.log());
    made.pointer("/agentfile/id").and_then(Value::as_str).map(|id| format!("agentfile:{id}")).ok_or_else(|| anyhow::anyhow!("no id: {made}"))
}

/// P4.7 AC2 and AC4: an Agentfile names a person's secret by reference; Ada's start of hers takes
/// hers (what refuses it next is where it would run: no machine here starts microVMs), and Bo's of
/// the same text, naming a secret he holds none of, is refused on every start surface before anything
/// is placed, naming the reference and never a value. An Agentfile with a value where its reference
/// goes isn't kept, its refusal not repeating it. No answer or log carries a value.
#[test]
fn p4_7_a_build_takes_its_persons_secret_by_reference_and_no_one_elses() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let srv = start_env(dir.path(), &Options::default(), false, &[])?;
    let ada = person(&srv, "ada", &["developer", "runners.terminal"])?;
    let bo = person(&srv, "bo", &["developer", "runners.terminal"])?;
    let mut said = Said::default();
    let (st, _) = said.call(&srv, &ada, "PUT", "/api/me/secrets/GH_TOKEN", Some(json!({ "value": VALUE })))?;
    anyhow::ensure!(st == 200, "{st}");
    // Re-sealed under a new data key, each row under its own scope: hers still opens for her build below.
    let (st, rotated) = said.call(&srv, &srv.admin, "POST", "/api/ci/secrets/rotate", None)?;
    anyhow::ensure!(st == 200 && rotated.get("resealed").and_then(Value::as_u64).is_some_and(|n| n >= 1), "{st} {rotated}");
    let hers = agentfile(&mut said, &srv, &ada, "private-harness", CLAUDE_PRIVATE)?;
    let his = agentfile(&mut said, &srv, &bo, "private-harness", CLAUDE_PRIVATE)?;
    let session = |image: &str| json!({ "target": "agent:vmbox", "spec": { "kind": "claude", "isolation": { "kind": "microvm", "image": image } } });
    let not_his = |s: &str| s.contains("kiln.secret.GIT_AUTH_TOKEN names user:GH_TOKEN, which isn't one of your secrets");

    // Bo's start, on each surface, is refused for the secret he lacks; nothing starts for him.
    let (st, refused) = said.call(&srv, &bo, "POST", "/api/harness/sessions", Some(session(&his)))?;
    anyhow::ensure!(st == 400 && not_his(&refused.to_string()), "the sessions route: {st} {refused}");
    let (failed, out) = said.mcp(&srv, &bo, "agents_start", json!({ "kind": "claude", "image": his, "on": "local" }))?;
    anyhow::ensure!(failed && not_his(&out), "agents_start: {out}");
    let (_, listed) = said.call(&srv, &srv.admin, "GET", "/api/harness/sessions", None)?;
    anyhow::ensure!(listed == json!([]), "nothing started: {listed}");

    // Ada's passes: hers is read for its build; what stops it here is that no machine starts microVMs.
    let (st, out) = said.call(&srv, &ada, "POST", "/api/harness/sessions", Some(session(&hers)))?;
    anyhow::ensure!(!out.to_string().contains("isn't one of your secrets"), "the sessions route, hers: {st} {out}");
    let (failed, out) = said.mcp(&srv, &ada, "agents_start", json!({ "kind": "claude", "image": hers, "on": "local" }))?;
    anyhow::ensure!(failed && !out.contains("isn't one of your secrets") && out.contains("microVM"), "agents_start, hers: {out}");
    // Her secret gone, her start is refused as Bo's is.
    let (st, _) = said.call(&srv, &ada, "DELETE", "/api/me/secrets/GH_TOKEN", None)?;
    anyhow::ensure!(st == 200, "{st}");
    let (failed, out) = said.mcp(&srv, &ada, "agents_start", json!({ "kind": "claude", "image": hers, "on": "local" }))?;
    anyhow::ensure!(failed && not_his(&out), "agents_start, hers deleted: {out}");

    // A value where its reference goes isn't kept, on create or check, and isn't repeated.
    let pasted = CLAUDE_PRIVATE.replace("user:GH_TOKEN", VALUE);
    let (st, refused) = said.call(&srv, &ada, "POST", "/api/agentfiles", Some(json!({ "name": "pasted", "text": pasted })))?;
    let words = refused.to_string();
    anyhow::ensure!(st == 422 && words.contains("never goes in an Agentfile") && !words.contains(VALUE), "{st} {words}");
    let (st, checked) = http(srv.port, "POST", "/api/agentfiles/check", &ada, Some(&json!({ "text": pasted })))?;
    anyhow::ensure!(st == 200 && checked.contains("SecretReference") && !checked.contains(VALUE), "{st} {checked}");
    // The kept text carries the reference alone.
    let (_, kept) = said.call(&srv, &ada, "GET", &format!("/api/agentfiles/{}", hers.trim_start_matches("agentfile:")), None)?;
    anyhow::ensure!(kept.pointer("/text").and_then(Value::as_str) == Some(CLAUDE_PRIVATE), "{kept}");
    said.none_carry(&srv, &[VALUE])
}
