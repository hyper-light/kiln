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

/// W26-AC10's fake agents, which kiln's fake harness runs with each message, in the agent's folder:
/// - `run <name>` runs `<name>.js` through the agent's own MCP server (`kiln reach mcp`, as its
///   session, so its runner signs its words), its answer in `<name>.out`;
/// - `edit <words>` adds a line to a.py, as an edit kiln counts (`.fake-claude-edited`);
/// - kiln's feedback (it says why kiln sent it) is kept for the test as `fed-<n>`, and makes the edit
///   it asks for, as an agent would before it pushes.
fn ac10_act(dir: &Path) -> anyhow::Result<String> {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join("act-ac10.sh");
    let script = r##"#!/bin/sh
text=$(cat)
[ -f session ] || exit 0
KILN_SESSION=$(cat session)
export KILN_SESSION
edit() {
  mkdir -p .fake-claude-before
  cp a.py .fake-claude-before/a.py
  printf '%s\n' "# $1" >> a.py
  echo a.py > .fake-claude-edited
}
case "$text" in
  "run "*)
    name=${text#run }
    python3 -I -c 'import json, sys
p = open(sys.argv[1]).read()
print(json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}}))
print(json.dumps({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "code_run", "arguments": {"program": p}}}))' "$name.js" > "$name.in"
    { cat "$name.in"; while ! grep -q '"id":2' "$name.part" 2>/dev/null; do sleep 0.2; done; } | '@KILN@' reach mcp > "$name.part" 2> "$name.err"
    mv "$name.part" "$name.out"
    ;;
  "edit "*) edit "${text#edit }" ;;
  *"Why kiln sent you this"*)
    n=$(ls fed-* 2>/dev/null | wc -l | tr -d ' ')
    printf '%s\n' "$text" > "fed-$n.part"
    mv "fed-$n.part" "fed-$n"
    edit "fix $n"
    ;;
esac
"##
    .replace("@KILN@", env!("CARGO_BIN_EXE_kiln"));
    std::fs::write(&path, script)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path.to_string_lossy().into_owned())
}

/// One of W26-AC10's fake agents on box-a: its session, its folder and its name.
struct Fake {
    id: String,
    dir: std::path::PathBuf,
    name: String,
}

/// W26-AC10's world: acme/app, a repository kiln hosts with a.py on main; its agent fixes switch on
/// or off; a runner, box-a, whose agents are kiln's fake harness acting through [`ac10_act`]: the
/// Matrix agent, in a clone of the repository, whose change it is, and the review agent.
struct Ac10 {
    s: Server,
    _runner: agents::Runner,
    matrix: Fake,
    review: Fake,
    /// git's own folder, and the repository as its owner clones it.
    home: std::path::PathBuf,
    url: String,
    dir: tempfile::TempDir,
}

impl Ac10 {
    fn new(agents_fix: bool) -> anyhow::Result<Ac10> {
        let dir = tempfile::tempdir()?;
        let s = start(dir.path(), &Options::default())?;
        let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&serde_json::json!({"name": "acme/app", "mode": "native"})))?;
        anyhow::ensure!(st == 200, "a repository kiln hosts: {st} {body}");
        let home = dir.path().join("githome");
        std::fs::create_dir_all(&home)?;
        let url = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
        Ac10::git_in(&home, dir.path(), &["clone", "-q", &url, "base"])?;
        let base = dir.path().join("base");
        std::fs::write(base.join("a.py"), "def legs(matrix):\n    return [1]\n")?;
        Ac10::git_in(&home, &base, &["add", "."])?;
        Ac10::git_in(&home, &base, &["commit", "-qm", "legs"])?;
        Ac10::git_in(&home, &base, &["push", "-q", "origin", "main"])?;
        if agents_fix {
            let (st, body) = http(s.port, "PUT", "/api/repos/acme%2Fapp/agent-fixes", &s.admin, Some(&serde_json::json!({"agents_fix": true})))?;
            anyhow::ensure!(st == 200, "the switch: {st} {body}");
        }
        let act = ac10_act(dir.path())?;
        let (runner, log) = agents::runner_with(dir.path(), s.port, "box-a", None, TALK, &[("FAKE_CLAUDE_ACT", &act)])?;
        agents::connected(s.port, &s.admin, "box-a", &log, Duration::from_secs(60))?;
        Ac10::git_in(&home, dir.path(), &["clone", "-q", &url, "matrix"])?;
        let review = dir.path().join("review");
        std::fs::create_dir_all(&review)?;
        let matrix = Ac10::fake(&s, &dir.path().join("matrix"))?;
        let review = Ac10::fake(&s, &review)?;
        Ok(Ac10 { s, _runner: runner, matrix, review, home, url, dir })
    }

    fn git_in(home: &Path, at: &Path, args: &[&str]) -> anyhow::Result<String> {
        let o = std::process::Command::new("git")
            .args(["-c", "user.name=ada", "-c", "user.email=ada@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
            .args(args)
            .current_dir(at)
            .env("HOME", home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()?;
        let said = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        anyhow::ensure!(o.status.success(), "git {args:?}: {said}");
        Ok(said)
    }

    /// The repository, cloned by its owner into folder `name`.
    fn clone_to(&self, name: &str) -> anyhow::Result<std::path::PathBuf> {
        Ac10::git_in(&self.home, self.dir.path(), &["clone", "-q", &self.url, name])?;
        Ok(self.dir.path().join(name))
    }

    fn fake(s: &Server, cwd: &Path) -> anyhow::Result<Fake> {
        let id = agents::open_session(s.port, &s.admin, "box-a", cwd, Duration::from_secs(60))?;
        Ac10::fake_of(s, cwd, id)
    }

    /// Agent session `id`, working in `cwd`, as one of W26-AC10's fake agents.
    fn fake_of(s: &Server, cwd: &Path, id: String) -> anyhow::Result<Fake> {
        std::fs::write(cwd.join(".fake-claude-messages"), "")?;
        std::fs::write(cwd.join("session"), &id)?;
        until_identified(s, &id, Duration::from_secs(60))?;
        let (st, body) = http(s.port, "GET", &format!("/api/harness/sessions/{id}"), &s.admin, None)?;
        anyhow::ensure!(st == 200, "the session: {st} {body}");
        let name = field(&serde_json::from_str::<Value>(&body)?, "name").to_owned();
        Ok(Fake { id, dir: cwd.to_path_buf(), name })
    }

    fn get(&self, path: &str) -> anyhow::Result<Value> {
        let (st, body) = http(self.s.port, "GET", path, &self.s.admin, None)?;
        anyhow::ensure!(st == 200, "{path}: {st} {body}");
        Ok(serde_json::from_str(&body)?)
    }

    fn send(&self, a: &Fake, text: &str) -> anyhow::Result<()> {
        static SENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = format!("ac10-{}", SENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let (st, body) = http(self.s.port, "POST", &format!("/api/harness/sessions/{}/command", a.id), &self.s.admin, Some(&serde_json::json!({"type": "send", "id": id, "text": text})))?;
        anyhow::ensure!((200..300).contains(&st), "send to {}: {st} {body}", a.name);
        Ok(())
    }

    /// Agent `a` runs `program` through its own MCP server: what it returned, or why not.
    fn run(&self, a: &Fake, name: &str, program: &str) -> anyhow::Result<Value> {
        std::fs::write(a.dir.join(format!("{name}.js")), program)?;
        self.send(a, &format!("run {name}"))?;
        let out = a.dir.join(format!("{name}.out"));
        let deadline = Instant::now() + Duration::from_secs(90);
        while !out.exists() {
            anyhow::ensure!(Instant::now() < deadline, "{}'s {name} never answered: {}", a.name, std::fs::read_to_string(a.dir.join(format!("{name}.err"))).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(200));
        }
        let text = std::fs::read_to_string(&out)?;
        let answer: Value = text.lines().find(|l| l.contains("\"id\":2")).map(serde_json::from_str::<Value>).transpose()?.unwrap_or_default();
        let structured = answer.pointer("/result/structuredContent").cloned().unwrap_or_default();
        anyhow::ensure!(answer.pointer("/result/isError") != Some(&Value::Bool(true)), "{}'s {name}: {structured}", a.name);
        Ok(structured.get("result").cloned().unwrap_or_default())
    }

    /// The Matrix agent's lines added, as kiln counts its edits.
    fn lines_added(&self) -> anyhow::Result<u64> {
        Ok(self.get(&format!("/api/harness/sessions/{}", self.matrix.id))?.get("lines_added").and_then(Value::as_u64).unwrap_or_default())
    }

    /// Until kiln counts more of the Matrix agent's lines than `before`.
    fn until_edited(&self, before: u64) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.lines_added()? <= before {
            anyhow::ensure!(Instant::now() < deadline, "kiln never counted the Matrix agent's edit");
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(())
    }

    /// The Matrix agent opens its change request (an edit, then `agents.change`): its name.
    fn open(&self, draft: bool) -> anyhow::Result<String> {
        let before = self.lines_added()?;
        self.send(&self.matrix, "edit first")?;
        self.until_edited(before)?;
        let r = self.run(&self.matrix, "open", &format!("return await kiln.agents.change({{title: \"legs for a matrix\", draft: {draft}}});"))?;
        let name = r.pointer("/change_request/change").and_then(Value::as_str).unwrap_or_default().to_owned();
        anyhow::ensure!(name.starts_with("CR"), "a change request of the Matrix agent's: {r}");
        Ok(name)
    }

    /// The review agent comments on a.py line 2 of `cr`: its comment's id and its thread.
    fn comment(&self, cr: &str, name: &str, body: &str) -> anyhow::Result<(String, String)> {
        let c = self.run(&self.review, name, &format!("return await kiln.ci.diffs.comment({{diff: \"{cr}\", body: \"{body}\", file: \"a.py\", start_line: 2, publish: true}});"))?;
        let (id, thread) = (field(&c, "id").to_owned(), field(&c, "thread").to_owned());
        anyhow::ensure!(!id.is_empty() && !thread.is_empty(), "the review agent's comment: {c}");
        Ok((id, thread))
    }

    /// A reply of the review agent's in `thread`.
    fn reply(&self, cr: &str, thread: &str, name: &str, body: &str) -> anyhow::Result<()> {
        self.run(&self.review, name, &format!("return await kiln.ci.diffs.reply({{diff: \"{cr}\", thread: \"{thread}\", body: \"{body}\", publish: true}});"))?;
        Ok(())
    }

    /// What kiln has sent the Matrix agent so far, each message's text.
    fn heard(&self) -> Vec<String> {
        std::fs::read_to_string(self.matrix.dir.join(".fake-claude-messages"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .map(|m| match m.pointer("/message/content") {
                Some(Value::String(t)) => t.clone(),
                Some(Value::Array(blocks)) => blocks.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
                _ => String::new(),
            })
            .collect()
    }

    /// Until kiln's feedback number `n` (from 0) has reached the Matrix agent and its edit is
    /// counted: what kiln said.
    fn fed(&self, n: usize, edited_before: u64) -> anyhow::Result<String> {
        let at = self.matrix.dir.join(format!("fed-{n}"));
        let deadline = Instant::now() + Duration::from_secs(60);
        while !at.exists() {
            anyhow::ensure!(Instant::now() < deadline, "kiln's feedback {n} never reached the Matrix agent: {:?}", self.heard());
            std::thread::sleep(Duration::from_millis(200));
        }
        self.until_edited(edited_before)?;
        Ok(std::fs::read_to_string(at)?)
    }

    /// The Matrix agent pushes the fix it made (`agents.change` answering `thread`) and replies in
    /// the thread: the new version.
    fn fix(&self, cr: &str, thread: &str, name: &str) -> anyhow::Result<u64> {
        let r = self.run(
            &self.matrix,
            name,
            &format!("const c = await kiln.agents.change({{answers: [\"{thread}\"]}}); await kiln.ci.diffs.reply({{diff: \"{cr}\", thread: \"{thread}\", body: \"Fixed in v\" + c.change_request.version + \": no legs now.\", publish: true}}); return c;"),
        )?;
        r.pointer("/change_request/version").and_then(Value::as_u64).ok_or_else(|| anyhow::anyhow!("the fix's version: {r}"))
    }

    /// `cr`'s versions, its state and threads.
    fn change(&self, cr: &str) -> anyhow::Result<(Vec<Value>, String, Vec<Value>)> {
        let d = self.get(&format!("/api/diffs/{cr}"))?;
        let versions = d.get("versions").and_then(Value::as_array).cloned().unwrap_or_default();
        let threads = self.get(&format!("/api/diffs/{cr}/comments"))?;
        let threads = threads.get("threads").or(Some(&threads)).and_then(Value::as_array).cloned().unwrap_or_default();
        Ok((versions, field(&d, "state").to_owned(), threads))
    }

    /// How Ship reads `cr`'s newest version: its stand and line.
    fn ship(&self, cr: &str) -> anyhow::Result<(String, String)> {
        let (versions, ..) = self.change(cr)?;
        let tip = versions.last().map(|v| field(v, "commit").to_owned()).unwrap_or_default();
        let page = self.get("/api/ship/commits?limit=50")?;
        let item = page.get("commits").and_then(Value::as_array).into_iter().flatten().find(|c| tip.starts_with(field(c, "short")) && !field(c, "short").is_empty()).cloned();
        let item = item.ok_or_else(|| anyhow::anyhow!("no Ship item for {cr}'s {tip}: {page}"))?;
        Ok((field(&item, "stand").to_owned(), field(&item, "line").to_owned()))
    }
}

/// Until `f` holds, or `within`: why not, when it doesn't.
fn until(within: Duration, what: &str, mut f: impl FnMut() -> anyhow::Result<Option<String>>) -> anyhow::Result<()> {
    let deadline = Instant::now() + within;
    loop {
        match f()? {
            None => return Ok(()),
            Some(why) if Instant::now() >= deadline => anyhow::bail!("{what}: {why}"),
            Some(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

/// W26-AC10 with the switch off (the default): the review agent comments on the Matrix agent's
/// change request, and nothing comes of it. The Matrix agent's session records none of its words
/// (a person's comment after them reaches it: by then kiln had decided the review agent's), and
/// Ship's item doesn't read fixing.
#[test]
fn w26_ac10_the_switch_off_starts_nothing() -> anyhow::Result<()> {
    let w = Ac10::new(false)?;
    let cr = w.open(false)?;
    let (_, thread) = w.comment(&cr, "comment", "an empty matrix should mean no legs, not one")?;
    let (st, body) = http(w.s.port, "POST", &format!("/api/diffs/{cr}/comments"), &w.s.admin, Some(&serde_json::json!({"thread": thread, "body": "over to you", "publish": true})))?;
    anyhow::ensure!(st == 200, "a person's reply: {st} {body}");
    until(Duration::from_secs(60), "the person's words reach the Matrix agent", || Ok((!w.heard().iter().any(|t| t.contains("> over to you"))).then(|| format!("{:?}", w.heard()))))?;
    anyhow::ensure!(!w.heard().iter().any(|t| t.contains("> an empty matrix should mean no legs")), "the review agent's words reach nowhere: {:?}", w.heard());
    anyhow::ensure!(!w.matrix.dir.join("fed-0").exists(), "no fix starts");
    let (stand, line) = w.ship(&cr)?;
    anyhow::ensure!(!line.contains("fixing") && stand != "checking", "Ship doesn't read it fixing: {stand} {line}");
    Ok(())
}

/// W26-AC10 on a change request, the switch on: the review agent comments on the Matrix agent's
/// change request; the Matrix agent gets it with its thread, a.py line 2, its version and why kiln
/// sent it, and Ship reads it fixing; it pushes v2 answering the thread and replies there, signed;
/// v2's why is an agent's fix naming the thread, the thread says fixed in v2, and Ship no longer
/// reads fixing; the review agent resolves the thread.
#[test]
fn w26_ac10_comment_fix_answer_resolve() -> anyhow::Result<()> {
    comment_fix_answer_resolve(false)
}

/// W26-AC10 on a draft: the same, on the Matrix agent's draft change request, which stays a draft
/// through the fix's version until a person marks it ready.
#[test]
fn w26_ac10_a_draft_stays_a_draft_through_the_fix() -> anyhow::Result<()> {
    comment_fix_answer_resolve(true)
}

fn comment_fix_answer_resolve(draft: bool) -> anyhow::Result<()> {
    let w = Ac10::new(true)?;
    let cr = w.open(draft)?;
    comment_fix_answer_resolve_on(&w, &cr, draft)
}

/// W26-AC10's flow on the Matrix agent's change `cr`, the switch on: comment, fix, answer, resolve.
fn comment_fix_answer_resolve_on(w: &Ac10, cr: &str, draft: bool) -> anyhow::Result<()> {
    let cr = cr.to_owned();
    let (_, state, _) = w.change(&cr)?;
    anyhow::ensure!(state == if draft { "draft" } else { "needs-review" }, "{cr} as it starts: {state}");
    let edited = w.lines_added()?;
    let (comment, thread) = w.comment(&cr, "comment", "an empty matrix should mean no legs, not one")?;
    let said = w.fed(0, edited)?;
    anyhow::ensure!(said.contains("> an empty matrix should mean no legs, not one") && said.contains(&thread) && said.contains("a.py line 2 (version 1"), "with its thread, lines and version: {said}");
    anyhow::ensure!(said.contains("Why kiln sent you this: acme/app lets agents fix code from agents' feedback (turned on by admin"), "with why: {said}");
    anyhow::ensure!(!draft || said.contains("your draft change"), "a draft says so: {said}");
    // A draft stays a draft (W26-R02), naming who is at work on it.
    let (want_stand, want_start) = if draft { ("waiting", format!("Draft · {} fixing · ", w.matrix.name)) } else { ("checking", format!("{} fixing · ", w.matrix.name)) };
    until(Duration::from_secs(60), "Ship reads it fixing", || {
        let Ok((stand, line)) = w.ship(&cr) else { return Ok(Some("no Ship item yet".to_owned())) };
        Ok((!(stand == want_stand && line.starts_with(&want_start) && line.ends_with("'s thread"))).then(|| format!("{stand} {line}")))
    })?;

    let v = w.fix(&cr, &thread, "fix")?;
    anyhow::ensure!(v == 2, "the fix is v2: {v}");
    let (versions, state, threads) = w.change(&cr)?;
    let v2 = versions.iter().find(|x| x.get("n").and_then(Value::as_u64) == Some(2)).cloned().unwrap_or_default();
    anyhow::ensure!(field(&v2, "why") == "agent's fix" && field(&v2, "thread") == thread, "v2 is an agent's fix naming the thread: {v2}");
    let t = threads.iter().find(|t| field(t, "id") == thread).cloned().unwrap_or_default();
    anyhow::ensure!(t.get("fixed_in").and_then(Value::as_u64) == Some(2), "the thread says fixed in v2: {t}");
    // Its reply, as its own identity signed it (two agents begun within a minute on one runner share a principal).
    let signed_by = format!("/agent/{}/", w.matrix.id);
    let reply = t.get("comments").and_then(Value::as_array).into_iter().flatten().find(|c| c.pointer("/signed/by").and_then(Value::as_str).is_some_and(|b| b.contains(&signed_by))).cloned().unwrap_or_default();
    let signer = reply.pointer("/signed/by").and_then(Value::as_str).unwrap_or_default();
    anyhow::ensure!(field(&reply, "body").starts_with("Fixed in v2") && signer.contains(&format!("/agent/{}/", w.matrix.id)), "its reply in the thread, signed as the Matrix agent: {reply}");
    until(Duration::from_secs(60), "the new version ends the hand-off", || {
        let Ok((stand, line)) = w.ship(&cr) else { return Ok(Some("no Ship item yet".to_owned())) };
        Ok(line.contains("fixing").then(|| format!("{stand} {line}")))
    })?;
    anyhow::ensure!(state == if draft { "draft" } else { "needs-review" }, "{cr} after the fix: {state}");
    anyhow::ensure!(!w.matrix.dir.join("fed-1").exists(), "its own reply went nowhere: {:?}", w.heard());

    w.run(&w.review, "resolve", &format!("return await kiln.ci.diffs.resolve({{comment: \"{comment}\"}});"))?;
    let (_, _, threads) = w.change(&cr)?;
    let t = threads.iter().find(|t| field(t, "id") == thread).cloned().unwrap_or_default();
    anyhow::ensure!(field(&t, "state") == "resolved", "the review agent resolved it: {t}");
    if draft {
        let (st, body) = http(w.s.port, "POST", &format!("/api/diffs/{cr}/ready"), &w.s.admin, Some(&serde_json::json!({})))?;
        anyhow::ensure!(st == 200, "marked ready: {st} {body}");
        let (_, state, _) = w.change(&cr)?;
        anyhow::ensure!(state == "needs-review", "ready for review once marked: {state}");
    }
    Ok(())
}

/// W26-AC10's bound: with the switch on, the review agent and the Matrix agent keep answering each
/// other in one thread. kiln sends the Matrix agent the review agent's first three comments there,
/// each fixed in a version answering the thread; then the thread says it waits for a person, and the
/// fourth reaches no one. A person's reply there reaches the agent and starts the count again: the
/// review agent's next comment goes.
#[test]
fn w26_ac10_agents_fix_one_thread_at_most_three_times() -> anyhow::Result<()> {
    let w = Ac10::new(true)?;
    let cr = w.open(false)?;
    let mut edited = w.lines_added()?;
    let (_, thread) = w.comment(&cr, "comment", "an empty matrix should mean no legs")?;
    for n in 0..3u64 {
        if n > 0 {
            w.reply(&cr, &thread, &format!("again-{n}"), &format!("still one leg, take {n}"))?;
        }
        let said = w.fed(usize::try_from(n)?, edited)?;
        anyhow::ensure!(said.contains(&format!("Agents have started {n} of 3 fixes in thread {thread}")), "fix {n}, counted: {said}");
        let v = w.fix(&cr, &thread, &format!("fix-{n}"))?;
        anyhow::ensure!(v == n + 2, "fix {n} is v{}: {v}", n + 2);
        edited = w.lines_added()?;
    }
    let (versions, _, threads) = w.change(&cr)?;
    let fixes = versions.iter().filter(|v| field(v, "why") == "agent's fix" && field(v, "thread") == thread).count();
    anyhow::ensure!(fixes == 3, "three agents' fixes name the thread: {versions:?}");
    let t = threads.iter().find(|t| field(t, "id") == thread).cloned().unwrap_or_default();
    anyhow::ensure!(field(&t, "waits").contains("waits for a person"), "the thread waits for a person: {t}");

    w.reply(&cr, &thread, "again-3", "and once more")?;
    let (st, body) = http(w.s.port, "POST", &format!("/api/diffs/{cr}/comments"), &w.s.admin, Some(&serde_json::json!({"thread": thread, "body": "over to me", "publish": true})))?;
    anyhow::ensure!(st == 200, "a person's reply: {st} {body}");
    until(Duration::from_secs(60), "the person's words reach the Matrix agent", || Ok((!w.heard().iter().any(|t| t.contains("> over to me"))).then(|| format!("{:?}", w.heard()))))?;
    anyhow::ensure!(!w.matrix.dir.join("fed-3").exists() && !w.heard().iter().any(|t| t.contains("> and once more")), "the fourth reached no one: {:?}", w.heard());
    let (_, _, threads) = w.change(&cr)?;
    let t = threads.iter().find(|t| field(t, "id") == thread).cloned().unwrap_or_default();
    anyhow::ensure!(t.get("waits").is_none_or(Value::is_null), "a person spoke: it doesn't wait: {t}");
    w.reply(&cr, &thread, "again-4", "now the zero-axis case")?;
    let said = w.fed(3, edited)?;
    anyhow::ensure!(said.contains("> now the zero-axis case") && said.contains("Agents have started 0 of 3 fixes"), "the count started again: {said}");
    Ok(())
}

/// W26-AC10: an author's agent that has ended is sent nothing. The switch on; the Matrix agent opens
/// its change request and ends; the review agent comments: nothing reaches the Matrix agent, and
/// Ship's item says the fix waits for a person to start or resume it.
#[test]
fn w26_ac10_an_ended_agent_is_sent_nothing() -> anyhow::Result<()> {
    let w = Ac10::new(true)?;
    let cr = w.open(false)?;
    let (st, body) = http(w.s.port, "DELETE", &format!("/api/harness/sessions/{}", w.matrix.id), &w.s.admin, None)?;
    anyhow::ensure!((200..300).contains(&st), "the Matrix agent ends: {st} {body}");
    let heard = w.heard().len();
    w.comment(&cr, "comment", "an empty matrix should mean no legs")?;
    until(Duration::from_secs(60), "Ship's item says why", || {
        let Ok((stand, line)) = w.ship(&cr) else { return Ok(Some("no Ship item yet".to_owned())) };
        Ok((!line.starts_with("The fix waits for a person to start or resume ")).then(|| format!("{stand} {line}")))
    })?;
    anyhow::ensure!(w.heard().len() == heard && !w.matrix.dir.join("fed-0").exists(), "nothing reached it: {:?}", w.heard());
    Ok(())
}

/// W26-AC10 on a stacked diff: the same on a diff of a stack its agent made, a plan run's (docs/
/// agentic.md P4). The Matrix agent does the plan's one step on box-a, in a clone of its own, and
/// meets it; the run ends as D1, the Matrix agent's. Then the flow as on a change request: the
/// review agent comments on D1, the Matrix agent pushes D1's v2 answering the thread (its run's
/// stack made again) and replies there, signed, and the review agent resolves it.
#[test]
fn w26_ac10_the_same_on_a_stacked_diff() -> anyhow::Result<()> {
    let w = Ac10::new(true)?;
    let cwd = w.clone_to("step")?;
    let (st, body) = http(w.s.port, "POST", "/api/plans", &w.s.admin, Some(&serde_json::json!({"title": "Matrix legs", "body": "", "items": [{"title": "No legs for an empty matrix"}]})))?;
    anyhow::ensure!((200..300).contains(&st), "a plan: {st} {body}");
    let plan = field(&serde_json::from_str::<Value>(&body)?, "id").to_owned();
    let run = serde_json::json!({"id": plan, "mode": "agents", "new": {"on": "box-a", "kind": "claude", "cwd": cwd.to_string_lossy()}});
    let (st, body) = http(w.s.port, "POST", &format!("/api/plans/{plan}/run"), &w.s.admin, Some(&run))?;
    anyhow::ensure!((200..300).contains(&st), "its run, one agent per step: {st} {body}");
    let run = field(&serde_json::from_str::<Value>(&body)?, "run").to_owned();
    let mut agent = String::new();
    until(Duration::from_secs(60), "the step's agent starts", || {
        let v = w.get(&format!("/api/plans/runs/{run}"))?;
        agent = v.pointer("/steps/0/agent_id").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok(agent.is_empty().then(|| v.to_string()))
    })?;
    let step = Ac10::fake_of(&w.s, &cwd, agent)?;
    std::fs::write(step.dir.join(".fake-claude-messages"), "")?;
    let w = Ac10 { matrix: step, ..w };
    // It does its step (an edit), and meets it: the run ends as a stack of one diff, its own.
    let before = w.lines_added()?;
    w.send(&w.matrix, "edit the step")?;
    w.until_edited(before)?;
    // Its step is a goal on its stack: met by its id.
    let done = w.run(
        &w.matrix,
        "done",
        "const g = await kiln.goals.get(); const open = (g.items || []).filter(i => i.status === \"active\" || i.status === \"pending\"); if (open.length === 0) { return {stack: g}; } return await kiln.goals.done({id: open[0].id, evidence: \"no legs for an empty matrix\"});",
    )?;
    anyhow::ensure!(done.get("stack").is_none(), "the step's goal, on its agent's stack: {done}");
    let mut diff = String::new();
    until(Duration::from_secs(90), "the run ends as a diff of the Matrix agent's", || {
        let v = w.get(&format!("/api/plans/runs/{run}"))?;
        let changes = w.get(&format!("/api/harness/changes?agent={}", w.matrix.id))?;
        diff = changes.pointer("/changes/0/change").or_else(|| changes.pointer("/0/change")).and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok((field(&v, "state") != "done" || !diff.starts_with('D')).then(|| format!("{v} {changes}")))
    })?;
    let d = w.get(&format!("/api/diffs/{diff}"))?;
    anyhow::ensure!(field(&d, "kind") == "diff" && field(&d, "author").contains(&w.matrix.id), "a stacked diff, the Matrix agent's: {d}");
    comment_fix_answer_resolve_on(&w, &diff, false)
}
