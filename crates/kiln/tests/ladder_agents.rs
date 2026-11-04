//! The conflict ladder's agents (docs/ci.md W26-R17), end to end with kiln's fake harness
//! (`tests/fixtures/fake-claude`, acting through `FAKE_CLAUDE_ACT`): a change an agent made, whose
//! conflict kiln's merge and memory don't settle, is handed to that agent; it clones the repository
//! into a workspace of its own, resolves, runs the tests there and answers with why; kiln commits
//! it as the agent, tests it again, and with automatic resolution off it waits for a person. A
//! person's change in a repository that sends agents and resolves automatically gets an agent kiln
//! starts on a runner, and its tested resolution goes on by itself.

#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, Server, http, start_env};
use serde_json::{Value, json};

fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

fn show(bare: &Path, rev: &str, file: &str) -> anyhow::Result<String> {
    let o = Command::new("git").arg("-C").arg(bare).args(["show", &format!("{rev}:{file}")]).env("GIT_CONFIG_NOSYSTEM", "1").output()?;
    anyhow::ensure!(o.status.success(), "{rev}:{file}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

fn token(s: &Server, body: Value) -> anyhow::Result<String> {
    let (st, text) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&body))?;
    anyhow::ensure!((200..300).contains(&st), "a token: {st}");
    serde_json::from_str::<Value>(&text)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token"))
}

fn person(s: &Server, name: &str) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": ["developer@repo:acme/app"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    token(s, json!({ "kind": "pat", "user": name, "grants": ["owner"] }))
}

fn ok(s: &Server, token: &str, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<Value> {
    let (st, text) = http(s.port, method, path, token, body)?;
    let v = serde_json::from_str(&text).unwrap_or(Value::String(text));
    anyhow::ensure!(st == 200, "{method} {path}: {st} {v}");
    Ok(v)
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn until(secs: u64, what: &str, mut f: impl FnMut() -> anyhow::Result<Option<String>>) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match f()? {
            None => return Ok(()),
            Some(last) => anyhow::ensure!(Instant::now() < deadline, "{what}: {last}"),
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn expand(value: &str) -> String {
    format!("// plan/expand.rs\nfn one() {{\n    let a = {value};\n    more();\n}}\n")
}

const GATE: &str = r#"name: gate
on: [merge_group]
jobs:
  gate:
    runs-on: local
    steps:
      - env:
          TOKEN: ${{ github.token }}
        run: |
          git init -q .
          git -c http.extraHeader="Authorization: Bearer $TOKEN" fetch -q --depth=1 "$GITHUB_SERVER_URL/$GITHUB_REPOSITORY.git" "$GITHUB_SHA"
          git checkout -q FETCH_HEAD
      - run: "! grep -q 'let a = 99;' plan/expand.rs"
      # A land candidate takes a while: main can move under it.
      - if: startsWith(github.ref, 'refs/kiln/land/')
        run: sleep 6
"#;

/// What the fake agent does with a message: the brief's facts (its ```json block); clones the
/// repository into the workspace the brief names, checks out the conflicted revision, puts its
/// resolution in place, runs the test there, and answers each conflict as the agent, with why.
const ACT: &str = r#"#!/usr/bin/env python3
import json, os, re, subprocess, sys, urllib.request
text = sys.stdin.read()
m = re.search(r"```json\n(.*?)\n```", text, re.S)
if not m:
    sys.exit(0)
f = json.loads(m.group(1))
token = open(os.environ["FAKE_ACT_TOKEN_FILE"]).read().strip()
ws = os.path.expanduser(f["workspace"])
os.makedirs(os.path.dirname(ws), exist_ok=True)
auth = ["-c", "http.extraHeader=Authorization: Bearer " + token]
subprocess.run(["git", *auth, "clone", "-q", f["clone"], ws], check=True)
subprocess.run(["git", *auth, "-C", ws, "fetch", "-q", "origin", f["ref"]], check=True)
subprocess.run(["git", "-C", ws, "checkout", "-q", "FETCH_HEAD"], check=True)
resolution = os.environ["FAKE_ACT_RESOLVE"]
for c in f["conflicts"]:
    path = os.path.join(ws, c["file"])
    body = open(path).read()
    fixed = re.sub(r"<<<<<<< [^\n]*\n.*?>>>>>>> [^\n]*\n", resolution + "\n", body, count=1, flags=re.S)
    open(path, "w").write(fixed)
    ran = subprocess.run(["sh", "-c", os.environ["FAKE_ACT_TEST"]], cwd=ws).returncode
    why = "Main's value and the change's together: 110. Ran `%s` in my workspace: %s." % (os.environ["FAKE_ACT_TEST"], "passed" if ran == 0 else "failed")
    req = urllib.request.Request(
        os.environ["FAKE_ACT_API"] + "/api/diffs/%s/conflicts/%s/resolution" % (f["diff"], c["id"]),
        data=json.dumps({"take": "edit", "text": resolution, "explanation": why}).encode(),
        headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"},
        method="POST",
    )
    urllib.request.urlopen(req).read()
open(os.path.join(ws, ".fake-agent-done"), "w").write(f["replay"])
"#;

struct World {
    s: Server,
    _runner: agents::Runner,
    bare: std::path::PathBuf,
    kim: String,
    token_file: std::path::PathBuf,
    home: std::path::PathBuf,
}

/// A server whose PATH has no harness, its own runner for the checks, and runner r1 with the
/// fake harness acting as ACT; acme/app with the gate and plan/expand.rs, main about to move.
fn world(dir: &Path) -> anyhow::Result<World> {
    world_with(dir, "")
}

/// [`world`], the trunk's `.kiln/review.yaml` with `more` after its land policy.
fn world_with(dir: &Path, more: &str) -> anyhow::Result<World> {
    let system = agents::path_without_harnesses(dir)?;
    let s = start_env(dir, &Options { agent: true, ..Options::default() }, false, &[("PATH", &system)])?;
    let act = dir.join("act.py");
    std::fs::write(&act, ACT)?;
    std::fs::set_permissions(&act, std::fs::Permissions::from_mode(0o755))?;
    let token_file = dir.join("agent.token");
    let api = format!("http://127.0.0.1:{}", s.port);
    let env = [
        ("FAKE_CLAUDE_ACT", act.to_string_lossy().into_owned()),
        ("FAKE_ACT_TOKEN_FILE", token_file.to_string_lossy().into_owned()),
        ("FAKE_ACT_API", api),
        ("FAKE_ACT_RESOLVE", "    let a = 110;".to_owned()),
        ("FAKE_ACT_TEST", "grep -q 'let a = 110;' plan/expand.rs".to_owned()),
    ];
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (runner, log) = agents::runner_with(dir, s.port, "r1", None, Talk { lines: 1, every_ms: 1 }, &env)?;
    agents::connected(s.port, &s.admin, "r1", &log, Duration::from_secs(120))?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let bare = std::path::PathBuf::from(text(&serde_json::from_str(&body)?, "clone_url").trim_start_matches("file://"));
    let kim = person(&s, "kim")?;
    let home = dir.join("githome");
    std::fs::create_dir_all(&home)?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&home, dir, &["clone", "-q", &url, "owner"])?;
    let owner = dir.join("owner");
    for (f, body) in [(".kiln/review.yaml", format!("land:\n  approvals: 1\n  resolve-threads: false\n{more}")), (".github/workflows/gate.yml", GATE.to_owned()), ("plan/expand.rs", expand("1"))] {
        if let Some(d) = owner.join(f).parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(owner.join(f), body)?;
    }
    git(&home, &owner, &["add", "-A"])?;
    git(&home, &owner, &["commit", "-qm", "first"])?;
    git(&home, &owner, &["push", "-q", "origin", "main"])?;
    Ok(World { s, _runner: runner, bare, kim, token_file, home })
}

impl World {
    /// D1 from the first trunk (one is 10), pushed with `token`; then main makes it 100.
    fn conflict(&self, dir: &Path, token: &str) -> anyhow::Result<()> {
        self.push_d1(dir, token)?;
        self.move_main(dir)
    }

    fn push_d1(&self, dir: &Path, token: &str) -> anyhow::Result<()> {
        let url = |t: &str| format!("http://x:{t}@127.0.0.1:{}/git/acme/app.git", self.s.port);
        git(&self.home, dir, &["clone", "-q", &url(token), "work"])?;
        let work = dir.join("work");
        std::fs::write(work.join("plan/expand.rs"), expand("10"))?;
        git(&self.home, &work, &["commit", "-qam", &format!("D1: one is ten\n\nChange-Id: I{}", "1".repeat(40))])?;
        git(&self.home, &work, &["push", "-q", "origin", "HEAD:refs/for/main"])?;
        Ok(())
    }

    fn move_main(&self, dir: &Path) -> anyhow::Result<()> {
        let owner = dir.join("owner");
        std::fs::write(owner.join("plan/expand.rs"), expand("100"))?;
        git(&self.home, &owner, &["commit", "-qam", "main moves"])?;
        git(&self.home, &owner, &["push", "-q", "origin", "HEAD:main"])?;
        Ok(())
    }

    fn entry(&self) -> anyhow::Result<Value> {
        let q = ok(&self.s, &self.kim, "GET", "/api/repos/acme%2Fapp/queue", None)?;
        Ok(q.get("entries").and_then(Value::as_array).and_then(|l| l.first()).cloned().unwrap_or_default())
    }

    fn ladder(&self) -> anyhow::Result<Value> {
        ok(&self.s, &self.kim, "GET", "/api/diffs/D1/conflicts/ladder", None)
    }

    fn agent_try(&self) -> anyhow::Result<Value> {
        let l = self.ladder()?;
        let tries: Vec<Value> = l.get("conflicts").and_then(Value::as_object).map(|m| m.values().flat_map(|v| v.as_array().cloned().unwrap_or_default()).collect()).unwrap_or_default();
        Ok(tries.into_iter().rfind(|t| text(t, "rung") == "agent").unwrap_or_default())
    }

    fn latest(&self) -> anyhow::Result<(u64, String)> {
        let v = ok(&self.s, &self.kim, "GET", "/api/diffs/D1", None)?;
        let last = v.get("versions").and_then(Value::as_array).and_then(|v| v.last()).cloned().unwrap_or_default();
        Ok((last.get("n").and_then(Value::as_u64).unwrap_or_default(), text(&last, "commit").to_owned()))
    }
}

/// W26-R17, W26-AC10's ladder: the change's author's agent is handed the conflict kiln's merge and
/// memory left; it resolves in a workspace of its own, runs the tests and says why; kiln commits it
/// as the agent and tests it; with automatic resolution off it waits for a person, and lands as D1's
/// new version only once kim accepts it.
#[test]
fn w26_r17_the_authors_agent_resolves() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let w = world(dir.path())?;
    // The agent: a session on r1, and its token, whose pushes are the agent's.
    let cwd = dir.path().join("agent-cwd");
    std::fs::create_dir_all(&cwd)?;
    let session = agents::open_session(w.s.port, &w.s.admin, "r1", &cwd, Duration::from_secs(120))?;
    let agent = token(&w.s, json!({ "kind": "agent", "agent": session, "user": "kim", "grants": ["developer@repo:acme/app"] }))?;
    std::fs::write(&w.token_file, &agent)?;
    w.conflict(dir.path(), &agent)?;

    let got = ok(&w.s, &w.kim, "GET", "/api/diffs/D1/conflicts", None)?;
    assert_eq!(got.pointer("/replay/state"), Some(&json!("conflicted")), "{got}");
    until(240, "the agent's resolution tested, and the whole offered", || {
        let l = w.ladder()?;
        Ok((text(&l, "state") != "offered").then(|| l.to_string()))
    })?;
    let t = w.agent_try()?;
    assert_eq!((text(&t, "state"), text(&t, "tests")), ("resolved", "passed"), "{t}");
    assert!(text(&t, "by").contains(&session), "committed as the agent: {t}");
    assert!(text(&t, "explanation").contains("in my workspace: passed"), "it ran the tests there and said so: {t}");
    assert_eq!(t.get("lines"), Some(&json!(["    let a = 110;"])), "{t}");
    // Its own workspace, under its home's cache, made from the conflicted revision.
    let replay = text(&w.ladder()?, "replay").to_owned();
    let done = dir.path().join("home/.cache/kiln/resolve/D1").join(&replay).join(".fake-agent-done");
    assert_eq!(std::fs::read_to_string(&done).unwrap_or_default(), replay, "its workspace: {}", done.display());
    // Files reads it from the conflicts themselves: each one's offered try, and the ladder's state.
    let got = ok(&w.s, &w.kim, "GET", "/api/diffs/D1/conflicts", None)?;
    assert_eq!((got.pointer("/conflicts/0/offered/rung"), got.pointer("/conflicts/0/offered/tests"), got.pointer("/ladder/state")), (Some(&json!("agent")), Some(&json!("passed")), Some(&json!("offered"))), "{got}");
    // Ship's row says the agent is on it (W26-R18's Fixing), on D1's latest revision.
    let (_, head) = w.latest()?;
    let page = ok(&w.s, &w.kim, "GET", "/api/ship/commits?repo=acme/app", None)?;
    let row = page.get("commits").and_then(Value::as_array).into_iter().flatten().find(|c| head.starts_with(text(c, "short")) && !text(c, "short").is_empty()).cloned().unwrap_or_default();
    assert!(text(&row, "line").contains("fixing"), "D1's row says its agent is fixing: {row}");
    // Waiting for a person: nothing lands by itself (its latest is still the conflicted replay kiln kept).
    let waiting = w.latest()?.0;
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(text(&w.ladder()?, "state"), "offered");
    assert_eq!(w.latest()?.0, waiting, "no new version until a person accepts");
    let r = ok(&w.s, &w.kim, "POST", "/api/diffs/D1/conflicts/accept", Some(&json!({})))?;
    assert_eq!(r.get("version"), Some(&json!(waiting + 1)), "{r}");
    let (_, commit) = w.latest()?;
    assert_eq!(show(&w.bare, &commit, "plan/expand.rs")?, expand("110"));
    Ok(())
}

/// W26-R17: a person's change, in a repository that sends agents and resolves automatically: kiln
/// starts an agent on a runner with the harness (r1), which resolves in its own workspace; once its
/// tests and then the whole pass, it's D1's new version without anyone accepting it, and says so.
#[test]
fn w26_r17_a_sent_agent_resolves_in_its_own_workspace() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // Sending agents is the trunk's own (W26-R16's setting); automatic resolution is the Repos page's.
    let w = world_with(dir.path(), "fresh:\n  conflict-agents: true\n")?;
    let dev = person(&w.s, "dev")?;
    // The agent kiln starts answers with a token of its own (the fake has none from kiln).
    std::fs::write(&w.token_file, token(&w.s, json!({ "kind": "agent", "agent": "resolver", "user": "dev", "grants": ["developer@repo:acme/app"] }))?)?;
    // Off by default; a developer can't turn it on.
    let sw = ok(&w.s, &w.kim, "GET", "/api/repos/acme%2Fapp/ladder", None)?;
    assert_eq!((sw.get("conflict_agents"), sw.get("automatic")), (Some(&json!(true)), Some(&json!(false))), "{sw}");
    let (st, _) = http(w.s.port, "PUT", "/api/repos/acme%2Fapp/ladder", &w.s.admin, Some(&json!({ "send_agent": true })))?;
    assert_eq!(st, 400, "sending agents is set in .kiln/review.yaml, not here");
    let (st, _) = http(w.s.port, "PUT", "/api/repos/acme%2Fapp/ladder", &w.kim, Some(&json!({ "automatic": true })))?;
    assert_eq!(st, 403);
    ok(&w.s, &w.s.admin, "PUT", "/api/repos/acme%2Fapp/ladder", Some(&json!({ "automatic": true })))?;
    w.conflict(dir.path(), &dev)?;

    ok(&w.s, &w.kim, "GET", "/api/diffs/D1/conflicts", None)?;
    until(240, "D1 resolved by itself", || {
        let l = w.ladder()?;
        let d = ok(&w.s, &w.kim, "GET", "/api/diffs/D1", None)?;
        let why = d.get("versions").and_then(Value::as_array).and_then(|v| v.last()).map(|v| text(v, "why").to_owned()).unwrap_or_default();
        Ok((why != "conflict resolved").then(|| l.to_string()))
    })?;
    let (_, commit) = w.latest()?;
    assert_eq!(show(&w.bare, &commit, "plan/expand.rs")?, expand("110"));
    let sessions: Value = serde_json::from_str(&http(w.s.port, "GET", "/api/harness/sessions", &w.s.admin, None)?.1).unwrap_or_default();
    let started = sessions.as_array().cloned().unwrap_or_default().into_iter().find(|s| text(s, "name") == "Resolve D1's conflicts").unwrap_or_default();
    // r1 as main names runners: by its machine id.
    let r1 = common::machine("r1");
    assert!(text(&started, "target").contains(&r1) || text(&started, "machine").contains(&r1) || started.to_string().contains(&r1), "on r1 ({r1}): {started}");
    Ok(())
}

/// W26-R17 in W28's queue: a conflicting entry goes back to Review (W28-R107), not out, and its diff
/// climbs the ladder. D1, the agent's, is approved and landing when main moves under its candidate:
/// the entry goes back to Review; the agent resolves it and its resolution is offered, the entry
/// still back with Review; kim accepts it, and once Review clears D1's new version (lead accepts
/// it: W28-R105, content Review hasn't cleared) the entry joins the queue again and lands.
#[test]
fn w26_r17_the_queue_holds_a_conflict_until_its_accepted() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let w = world(dir.path())?;
    let cwd = dir.path().join("agent-cwd");
    std::fs::create_dir_all(&cwd)?;
    let session = agents::open_session(w.s.port, &w.s.admin, "r1", &cwd, Duration::from_secs(120))?;
    let agent = token(&w.s, json!({ "kind": "agent", "agent": session, "user": "kim", "grants": ["developer@repo:acme/app"] }))?;
    std::fs::write(&w.token_file, &agent)?;
    w.push_d1(dir.path(), &agent)?;
    let lead = person(&w.s, "lead")?;
    ok(&w.s, &lead, "POST", "/api/diffs/D1/review", Some(&json!({ "action": "accept" })))?;
    ok(&w.s, &w.s.admin, "POST", "/api/diffs/D1/land", Some(&json!({})))?;
    until(120, "D1's candidate under test", || {
        let e = w.entry()?;
        Ok((text(&e, "state") != "testing").then(|| e.to_string()))
    })?;
    w.move_main(dir.path())?;
    until(240, "D1's entry back with Review", || {
        let e = w.entry()?;
        Ok((text(&e, "state") != "returned").then(|| e.to_string()))
    })?;
    until(240, "the agent's resolution offered", || {
        let l = w.ladder()?;
        Ok((text(&l, "state") != "offered").then(|| l.to_string()))
    })?;
    assert_eq!(text(&w.entry()?, "state"), "returned", "back with Review until a person accepts it");
    let (before, _) = w.latest()?;
    ok(&w.s, &w.kim, "POST", "/api/diffs/D1/conflicts/accept", Some(&json!({})))?;
    until(120, "the resolution is D1's new version", || {
        let (n, _) = w.latest()?;
        Ok((n <= before).then(|| format!("v{n}")))
    })?;
    ok(&w.s, &lead, "POST", "/api/diffs/D1/review", Some(&json!({ "action": "accept" })))?;
    until(300, "D1 lands", || {
        let d = ok(&w.s, &w.kim, "GET", "/api/diffs/D1", None)?;
        Ok((text(&d, "state") != "landed").then(|| format!("{} {}", text(&d, "state"), w.entry().map(|e| e.to_string()).unwrap_or_default())))
    })?;
    assert_eq!(show(&w.bare, "main", "plan/expand.rs")?, expand("110"));
    Ok(())
}

/// Goal 14's W26-R16 step 4, the dispatch the ladder carries out: with `fresh.conflict-agents` unset
/// (the default), a person's conflicting change starts no agent, and the ladder says why, naming the
/// setting; with it on, an agent starts on a runner with the harness (r1). (The on case resolving all
/// the way is w26_r17_a_sent_agent_resolves_in_its_own_workspace.)
#[test]
fn w26_r16_conflict_agent_only_when_on() -> anyhow::Result<()> {
    let started = |w: &World| -> anyhow::Result<Vec<Value>> {
        let sessions: Value = serde_json::from_str(&http(w.s.port, "GET", "/api/harness/sessions", &w.s.admin, None)?.1).unwrap_or_default();
        Ok(sessions.as_array().cloned().unwrap_or_default().into_iter().filter(|s| text(s, "name") == "Resolve D1's conflicts").collect())
    };

    // Off: the ladder holds the conflict for a person, saying the trunk sends no agent; none starts.
    let off = tempfile::tempdir()?;
    let w = world(off.path())?;
    let dev = person(&w.s, "dev")?;
    w.conflict(off.path(), &dev)?;
    ok(&w.s, &w.kim, "GET", "/api/diffs/D1/conflicts", None)?;
    until(240, "D1 held for a person", || {
        let l = w.ladder()?;
        Ok((text(&l, "state") != "held").then(|| l.to_string()))
    })?;
    let l = w.ladder()?;
    let why = l.get("why").map(Value::to_string).unwrap_or_default();
    assert!(why.contains("sends no agent (.kiln/review.yaml: fresh: conflict-agents)"), "{l}");
    assert_eq!(l.pointer("/switches/conflict_agents"), Some(&json!(false)), "{l}");
    assert!(w.agent_try()?.is_null(), "no agent tried: {l}");
    std::thread::sleep(Duration::from_secs(3));
    assert!(started(&w)?.is_empty(), "no agent started");
    drop(w);

    // On: an agent starts, on r1, the runner with the harness.
    let on = tempfile::tempdir()?;
    let w = world_with(on.path(), "fresh:\n  conflict-agents: true\n")?;
    let dev = person(&w.s, "dev")?;
    std::fs::write(&w.token_file, token(&w.s, json!({ "kind": "agent", "agent": "resolver", "user": "dev", "grants": ["developer@repo:acme/app"] }))?)?;
    w.conflict(on.path(), &dev)?;
    ok(&w.s, &w.kim, "GET", "/api/diffs/D1/conflicts", None)?;
    until(240, "an agent started for D1's conflict", || {
        let s = started(&w)?;
        Ok(s.is_empty().then(|| w.ladder().map(|l| l.to_string()).unwrap_or_default()))
    })?;
    let s = started(&w)?;
    let r1 = common::machine("r1");
    assert!(s.iter().any(|x| x.to_string().contains(&r1)), "on r1 ({r1}): {s:?}");
    Ok(())
}
