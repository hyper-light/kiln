//! Agents in review (docs/ci.md W26-R18), end to end with kiln's fake harness
//! (`tests/fixtures/fake-claude`, acting through `FAKE_CLAUDE_ACT`): an agent comments, suggests,
//! replies and gives verdicts the way a person does, through its MCP server (`kiln reach mcp`'s
//! `code_run`), signed as the agent for its person; and what it says reads back with who wrote it
//! and who signed it.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, Server, http, start};
use serde_json::Value;

const TALK: Talk = Talk { lines: 1, every_ms: 10 };

fn field<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

fn add_person(s: &Server, name: &str) -> anyhow::Result<()> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&serde_json::json!({ "name": name, "grants": ["developer"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    Ok(())
}

fn token_of(s: &Server, name: &str) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&serde_json::json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}'s token: {st} {body}");
    serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))
}

/// Waits until the agent session `id` has an identity kiln verifies (its runner asks for one as it
/// starts, W27-R01).
fn until_identified(s: &Server, id: &str, within: Duration) -> anyhow::Result<()> {
    let epoch = format!("/agent/{id}/epoch/");
    let deadline = Instant::now() + within;
    loop {
        let (st, body) = http(s.port, "GET", "/api/identities?kind=agent", &s.admin, None)?;
        anyhow::ensure!(st == 200, "{st} {body}");
        let list = serde_json::from_str::<Value>(&body)?.get("identities").and_then(Value::as_array).cloned().unwrap_or_default();
        if list.iter().any(|i| field(i, "subject").contains(&epoch) && !field(i, "verified").is_empty()) {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "agent {id} has no identity: {list:?}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// A script for the fake harness: given the message `<word> <its session's id>`, it runs
/// `program` through the agent's MCP server (`kiln reach mcp`, as `KILN_SESSION`), writing what it
/// answered to `mcp.out` and `done` once it has; other messages, kiln's own first one among them,
/// it leaves.
fn act(dir: &Path, word: &str, program: &str) -> anyhow::Result<String> {
    use std::os::unix::fs::PermissionsExt as _;
    let call = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "code_run", "arguments": {"program": program}}});
    let init = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}});
    let path = dir.join(format!("act-{word}.sh"));
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ntext=$(cat)\ncase \"$text\" in {word}\\ *) KILN_SESSION=${{text#{word} }} ;; *) exit 0 ;; esac\nexport KILN_SESSION\n{{ printf '%s\\n' '{init}' '{call}'; while ! grep -q '\"id\":2' mcp.out 2>/dev/null; do sleep 0.2; done; }} | '{}' reach mcp > mcp.out 2> mcp.err\necho done >> mcp.out\n",
            env!("CARGO_BIN_EXE_kiln")
        ),
    )?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path.to_string_lossy().into_owned())
}

/// W26-R18: an agent's words read like a person's, signed as the agent for its person. D1 is Sam's
/// change in a repository kiln hosts. Sam comments on it while the trunk only warns of unsigned
/// words; then the trunk requires signatures, and an agent comments with a suggestion, replies in
/// that thread and asks for changes, through its MCP server. Each is taken, signed by the runner
/// holding the agent's key. The threads read the agent's comment and reply back as written by its
/// session's name for its person and signed by its session's identity, and Sam's as Sam's,
/// unsigned; its verdict reads signed the same way.
#[test]
fn w26_r18_agents_words_are_signed_as_the_agent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&serde_json::json!({"name": "acme/agent-words", "mode": "native"})))?;
    anyhow::ensure!(st == 200, "a repository kiln hosts: {st} {body}");
    add_person(&s, "sam")?;
    let sam = token_of(&s, "sam")?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let git = |at: &Path, args: &[&str]| -> anyhow::Result<String> {
        let o = std::process::Command::new("git")
            .args(["-c", "user.name=sam", "-c", "user.email=sam@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
            .args(args)
            .current_dir(at)
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()?;
        let said = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        anyhow::ensure!(o.status.success(), "git {args:?}: {said}");
        Ok(said)
    };
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/agent-words.git", s.port);
    git(dir.path(), &["clone", "-q", &url(&sam), "work"])?;
    let work = dir.path().join("work");
    std::fs::write(work.join("a.py"), "def f():\n    return 1\n")?;
    git(&work, &["add", "."])?;
    git(&work, &["commit", "-qm", "first"])?;
    git(&work, &["push", "-q", "origin", "main"])?;
    git(&work, &["checkout", "-qb", "two"])?;
    std::fs::write(work.join("a.py"), "def f():\n    return 2\n")?;
    git(&work, &["commit", "-qam", "f returns two\n\nChange-Id: I1818181818181818181818181818181818181818\n"])?;
    let said = git(&work, &["push", "origin", "HEAD:refs/for/main"])?;
    anyhow::ensure!(said.contains("D1 v1"), "D1: {said}");

    // Sam's comment, while the trunk only warns: taken unsigned.
    let (st, body) = http(s.port, "POST", "/api/diffs/D1/comments", &sam, Some(&serde_json::json!({ "body": "why 2?", "file": "a.py", "start_line": 2, "publish": true })))?;
    anyhow::ensure!(st == 200, "sam's comment: {st} {body}");
    // From here the trunk requires signatures.
    git(&work, &["checkout", "-q", "main"])?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".kiln/review.yaml"), "signatures:\n  mode: required\n")?;
    git(&work, &["add", ".kiln/review.yaml"])?;
    git(&work, &["commit", "-qm", "signatures required"])?;
    git(&work, &["push", "-q", &url(&s.admin), "HEAD:main"])?;
    let (st, body) = http(s.port, "POST", "/api/diffs/D1/comments", &sam, Some(&serde_json::json!({ "body": "unsigned now?", "publish": true })))?;
    anyhow::ensure!(st == 428, "an unsigned comment where they're required: {st} {body}");

    // The agent: a comment with a suggestion on a line, a reply in its thread, a request for
    // changes, each published as it goes.
    let program = "const c = await kiln.ci.diffs.comment({diff: \"D1\", body: \"name the number\", file: \"a.py\", start_line: 2, suggestion: \"    return TWO\", publish: true}); const r = await kiln.ci.diffs.reply({diff: \"D1\", thread: c.thread, body: \"TWO = 2 above it\", publish: true}); const v = await kiln.ci.diffs.review({diff: \"D1\", action: \"request_changes\", body: \"a name for 2, please\"}); return {c, r, v};";
    let act_path = act(dir.path(), "words", program)?;
    let cwd = dir.path().join("agent-here");
    std::fs::create_dir_all(&cwd)?;
    let (_main, log) = agents::runner_with(dir.path(), s.port, "box-a", None, TALK, &[("FAKE_CLAUDE_ACT", &act_path)])?;
    agents::connected(s.port, &s.admin, "box-a", &log, Duration::from_secs(60))?;
    let id = agents::open_session(s.port, &s.admin, "box-a", &cwd, Duration::from_secs(60))?;
    until_identified(&s, &id, Duration::from_secs(60))?;
    let (st, sent) = http(s.port, "POST", &format!("/api/harness/sessions/{id}/command"), &s.admin, Some(&serde_json::json!({"type": "send", "id": "words", "text": format!("words {id}")})))?;
    anyhow::ensure!((200..300).contains(&st), "send: {st} {sent}");
    let out = || std::fs::read_to_string(cwd.join("mcp.out")).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !out().lines().any(|l| l.trim() == "done") {
        anyhow::ensure!(Instant::now() < deadline, "the agent's MCP server never answered: {}\n{}", out(), std::fs::read_to_string(cwd.join("mcp.err")).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(200));
    }

    // Its name, as kiln shows its session.
    let (st, body) = http(s.port, "GET", &format!("/api/harness/sessions/{id}"), &s.admin, None)?;
    anyhow::ensure!(st == 200, "the session: {st} {body}");
    let session: Value = serde_json::from_str(&body)?;
    let name = session.pointer("/info/name").or_else(|| session.get("name")).and_then(Value::as_str).unwrap_or_default().to_owned();
    anyhow::ensure!(!name.is_empty(), "the session has a name: {session}");

    let (st, body) = http(s.port, "GET", "/api/diffs/D1/comments", &s.admin, None)?;
    anyhow::ensure!(st == 200, "threads: {st} {body}");
    let threads: Value = serde_json::from_str(&body)?;
    let comments: Vec<Value> = threads.get("threads").or(Some(&threads)).and_then(Value::as_array).into_iter().flatten().flat_map(|t| t.get("comments").and_then(Value::as_array).cloned().unwrap_or_default()).collect();
    // The agent comments as its session does: `agent:<its id's start>@<its runner>`.
    let principal = format!("agent:{}@box-a", id.get(..8).unwrap_or(&id));
    let agents: Vec<&Value> = comments.iter().filter(|c| field(c, "author") == principal).collect();
    assert_eq!(agents.len(), 2, "the agent's comment and its reply, each taken\n{threads}\nthe agent's MCP server said: {}", out());
    for c in &agents {
        let by = c.get("by").cloned().unwrap_or_default();
        assert_eq!((field(&by, "kind"), field(&by, "name"), field(&by, "for")), ("agent", name.as_str(), "admin"), "written by the agent's session, for its person: {c}");
        let signed = c.get("signed").cloned().unwrap_or_default();
        let signer = kiln_ci::identity::chain::Identity::parse(field(&signed, "by")).map_err(|e| anyhow::anyhow!("{e}: {c}"))?;
        assert!(matches!(&signer.actor, kiln_ci::identity::chain::Actor::Agent { agent, person, .. } if *agent == id && person == "admin"), "signed as the agent, for its person: {c}");
        assert_eq!(field(&signed, "who"), format!("{name} for admin"), "{c}");
        assert!(signed.get("at").and_then(Value::as_i64).is_some_and(|t| t > 0), "when kiln took it: {c}");
    }
    let suggestion = agents.iter().find(|c| field(c, "body") == "name the number").copied().cloned().unwrap_or_default();
    assert_eq!(suggestion.pointer("/suggestions/0/lines/0").and_then(Value::as_str), Some("    return TWO"), "its suggestion, as a person's reads: {suggestion}");
    let sams = comments.iter().find(|c| field(c, "author") == "user:sam").cloned().unwrap_or_default();
    assert_eq!((sams.pointer("/by/kind"), sams.pointer("/by/name"), sams.get("signed")), (Some(&serde_json::json!("person")), Some(&serde_json::json!("sam")), Some(&Value::Null)), "a person's comment, unsigned: {sams}");

    // Its verdict, signed the same way.
    let (st, body) = http(s.port, "GET", "/api/diffs/D1/verdicts", &s.admin, None)?;
    anyhow::ensure!(st == 200, "verdicts: {st} {body}");
    let verdicts: Value = serde_json::from_str(&body)?;
    let v = verdicts.get("verdicts").and_then(Value::as_array).into_iter().flatten().find(|v| field(v, "action") == "request_changes").cloned().ok_or_else(|| anyhow::anyhow!("no request for changes: {body}\nthe agent's MCP server said: {}", out()))?;
    let signer = kiln_ci::identity::chain::Identity::parse(field(&v, "signer")).map_err(|e| anyhow::anyhow!("{e}: {v}"))?;
    assert!(matches!(&signer.actor, kiln_ci::identity::chain::Actor::Agent { agent, person, .. } if *agent == id && person == "admin"), "its verdict, as the agent for its person: {v}");
    Ok(())
}

/// W26-R18's switch, per repository: agents fix code from agents' feedback only where a person
/// with the land queue's capability (`diffs.land`) turned it on. Off by default, and saying so;
/// a developer reads it and is refused changing it; a maintainer turns it on and off, each kept
/// with who and when, and audited; another repository stays off.
#[test]
fn w26_r18_agents_fix_from_agents_feedback_only_where_turned_on() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    for name in ["acme/fixes", "acme/other"] {
        let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&serde_json::json!({"name": name, "mode": "native"})))?;
        anyhow::ensure!(st == 200, "{name}: {st} {body}");
    }
    let mint = |name: &str, grant: &str| -> anyhow::Result<String> {
        let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&serde_json::json!({ "name": name, "grants": [grant] })))?;
        anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
        let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&serde_json::json!({ "kind": "pat", "user": name, "grants": [grant] })))?;
        anyhow::ensure!((200..300).contains(&st), "{name}'s token: {st} {body}");
        serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))
    };
    let (dev, lead) = (mint("dev", "developer")?, mint("lead", "maintainer")?);
    let switch = |token: &str, repo: &str, body: Option<Value>| -> anyhow::Result<(u16, Value)> {
        // The repository's name is one segment, as the UI sends it (`acme%2Ffixes`).
        let path = format!("/api/repos/{}/agent-fixes", repo.replace('/', "%2F"));
        let (st, text) = http(s.port, if body.is_some() { "PUT" } else { "GET" }, &path, token, body.as_ref())?;
        Ok((st, serde_json::from_str(&text).unwrap_or(Value::String(text))))
    };
    let on = |v: &Value| v.get("agents_fix").and_then(Value::as_bool);
    let may = |v: &Value| v.get("may_change").and_then(Value::as_bool);

    let (st, v) = switch(&dev, "acme/fixes", None)?;
    anyhow::ensure!(st == 200 && on(&v) == Some(false) && may(&v) == Some(false) && v.get("set_by").is_some_and(Value::is_null), "off by default, and a developer can't change it: {st} {v}");
    anyhow::ensure!(field(&v, "says").contains("goes to that change's agent"), "it says what it does: {v}");
    let (st, v) = switch(&dev, "acme/fixes", Some(serde_json::json!({ "agents_fix": true })))?;
    anyhow::ensure!(st == 403 && v.to_string().contains("diffs.land"), "a developer is refused: {st} {v}");

    let (st, v) = switch(&lead, "acme/fixes", Some(serde_json::json!({ "agents_fix": "yes" })))?;
    anyhow::ensure!(st == 400, "true or false: {st} {v}");
    let (st, v) = switch(&lead, "acme/fixes", Some(serde_json::json!({ "agents_fix": true })))?;
    anyhow::ensure!(st == 200 && on(&v) == Some(true) && may(&v) == Some(true), "a maintainer turns it on: {st} {v}");
    anyhow::ensure!(field(&v, "set_by").contains("lead") && v.get("set_at").and_then(Value::as_i64).is_some_and(|t| t > 0), "who and when: {v}");
    let (st, v) = switch(&dev, "acme/fixes", None)?;
    anyhow::ensure!(st == 200 && on(&v) == Some(true) && field(&v, "set_by").contains("lead"), "the developer reads it on: {st} {v}");
    let (st, v) = switch(&dev, "acme/other", None)?;
    anyhow::ensure!(st == 200 && on(&v) == Some(false), "another repository stays off: {st} {v}");
    let (st, v) = switch(&lead, "acme/fixes", Some(serde_json::json!({ "agents_fix": false })))?;
    anyhow::ensure!(st == 200 && on(&v) == Some(false) && field(&v, "set_by").contains("lead"), "and off again, by whom: {st} {v}");
    let (st, v) = switch(&s.admin, "acme/none", None)?;
    anyhow::ensure!(st == 404, "no such repository: {st} {v}");

    // Each change is a note on its request's audit row (`audit::note`).
    let (st, audit) = http(s.port, "GET", "/api/audit?limit=200", &s.admin, None)?;
    anyhow::ensure!(st == 200, "{st} {audit}");
    for want in ["repo.agent_fixes: acme/fixes: agents fix from agents' feedback on", "repo.agent_fixes: acme/fixes: agents fix from agents' feedback off"] {
        anyhow::ensure!(audit.contains(want), "{want} in the audit: {audit}");
    }
    Ok(())
}
