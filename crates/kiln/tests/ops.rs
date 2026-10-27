//! W20-R01 (docs/ci.md): every operation over HTTP, in the OpenAPI document, and as `kiln op`,
//! all from the registry the MCP tools and code mode are made of. Each is listed with its input's
//! schema; run as its caller, it's checked as its own route is, and a change is audited once.
//! W20-AC03: an agent's authority is the same on every one of them.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, Server, http, start};
use serde_json::{Value, json};

#[test]
fn w20_r01_operations_over_http_and_the_cli() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;

    // Listed, each with whether it changes anything and its input's schema.
    let (st, body) = http(s.port, "GET", "/api/ops", &s.admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let list: Value = serde_json::from_str(&body)?;
    let find = |name: &str| list.get("operations").and_then(Value::as_array).and_then(|o| o.iter().find(|x| x.get("name") == Some(&json!(name)))).cloned().unwrap_or_default();
    let runs = find("ci.runs.list");
    assert!(runs.pointer("/input/type") == Some(&json!("object")) && runs.get("read_only") == Some(&json!(true)), "{runs}");
    assert_eq!(find("ci.variables.set").get("read_only"), Some(&json!(false)));
    assert_eq!(http(s.port, "GET", "/api/ops", "nobody", None)?.0, 401);

    // Run as its caller: the owner's change goes through, audited once, as its route's would be.
    let (st, body) = http(s.port, "GET", "/api/audit?limit=1", &s.admin, None)?;
    anyhow::ensure!(st == 200, "{body}");
    let before = serde_json::from_str::<Value>(&body)?.pointer("/0/id").and_then(Value::as_i64).unwrap_or(0);
    let (st, body) = http(s.port, "POST", "/api/ops/ci.variables.set", &s.admin, Some(&json!({ "scope": "server", "name": "REGION", "value": "eu" })))?;
    assert_eq!(st, 200, "{body}");
    let (_, body) = http(s.port, "GET", "/api/audit?limit=20", &s.admin, None)?;
    let rows: Vec<Value> = serde_json::from_str::<Vec<Value>>(&body)?.into_iter().filter(|r| r.get("id").and_then(Value::as_i64).is_some_and(|i| i > before)).collect();
    assert_eq!(rows.len(), 1, "one row: {rows:?}");
    assert!(rows.first().and_then(|r| r.get("target")).and_then(Value::as_str).is_some_and(|t| t.contains("/api/ci/variables/REGION")), "{rows:?}");
    let (st, body) = http(s.port, "POST", "/api/ops/ci.variables.list", &s.admin, Some(&json!({ "scope": "server" })))?;
    assert!(st == 200 && body.contains("REGION") && body.contains("eu"), "{st} {body}");
    let (st, body) = http(s.port, "POST", "/api/ops/ci.no.such", &s.admin, Some(&json!({})))?;
    assert_eq!(st, 404, "{body}");

    // A viewer's token reads, and is refused the change by its route's rule.
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": "ada", "grants": ["viewer"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": "ada", "grants": ["viewer"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{body}");
    let viewer = serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).unwrap_or_default().to_owned();
    assert_eq!(http(s.port, "POST", "/api/ops/ci.runs.list", &viewer, Some(&json!({})))?.0, 200);
    let (st, body) = http(s.port, "POST", "/api/ops/ci.variables.set", &viewer, Some(&json!({ "scope": "server", "name": "REGION", "value": "us" })))?;
    assert!(st == 403 && body.contains("variables.manage"), "{st} {body}");

    // In the OpenAPI document: each, with its summary and its input's schema.
    let (st, body) = http(s.port, "GET", "/api/openapi.json", &s.admin, None)?;
    anyhow::ensure!(st == 200, "{body}");
    let doc: Value = serde_json::from_str(&body)?;
    let op = doc.pointer("/paths/~1api~1ops~1ci.variables.set/post").cloned().unwrap_or_default();
    assert!(op.get("summary").and_then(Value::as_str).is_some_and(|s| !s.is_empty()), "{op}");
    assert_eq!(op.pointer("/requestBody/content/application~1json/schema/required"), Some(&json!(["scope", "name", "value"])), "{op}");

    // `kiln op`: the list, what one takes, and one run.
    let url = format!("http://127.0.0.1:{}", s.port);
    let kiln = |args: &[&str]| -> anyhow::Result<String> {
        let o = common::kiln().args(args).args(["--url", &url, "--token", &s.admin]).env("HOME", dir.path()).output()?;
        anyhow::ensure!(o.status.success(), "kiln {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let listed = kiln(&["op"])?;
    assert!(listed.lines().any(|l| l.starts_with("ci.variables.set") && l.contains("(changes)")), "{listed}");
    assert!(kiln(&["op", "ci.variables.set", "--schema"])?.contains("\"required\""));
    let set = kiln(&["op", "ci.variables.set", "scope=server", "name=ZONE", "value=b"])?;
    assert!(set.contains("\"set\": true"), "{set}");
    let read = kiln(&["op", "ci.variables.list", r#"{"scope": "server"}"#])?;
    assert!(read.contains("ZONE"), "{read}");
    Ok(())
}

/// W20-R03: a write to routing rules, a pool or an environment answers with what it does now: a
/// warning for a rule no runner connected now meets, for a pool whose executor no runner can start, and for a
/// pool whose labels a runner carries itself; and what each heads for.
#[test]
fn w20_r03_writes_say_what_they_do() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ..Options::default() })?;
    // The runner's facts: what it can run.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        let (_, body) = http(s.port, "GET", "/api/agents", &s.admin, None)?;
        if body.contains("\"executors\"") {
            break;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "no facts: {body}");
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    // No machine runs Plan 9: the first rule is met by none, whatever the hardware.
    let rules = json!({ "rules": [{ "match": { "labels": ["plan9"] }, "require": ["os == plan9"] }, { "match": { "labels": ["local"] }, "executor": ["host"] }] });
    let (st, body) = http(s.port, "POST", "/api/ops/ci.routing.set", &s.admin, Some(&rules))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    let warned: Vec<i64> = v.get("diagnostics").and_then(Value::as_array).into_iter().flatten().filter_map(|d| d.get("rule").and_then(Value::as_i64)).collect();
    assert_eq!(warned, vec![1], "only the rule no runner meets: {v}");
    assert!(v.get("preview").is_some_and(Value::is_array), "{v}");

    let pool = |spec: Value| -> anyhow::Result<Value> {
        let (st, body) = http(s.port, "POST", "/api/ops/ci.pools.set", &s.admin, Some(&spec))?;
        anyhow::ensure!(st == 200, "{st} {body}");
        Ok(serde_json::from_str(&body)?)
    };
    let vm = pool(json!({ "name": "vm", "executor": "firecracker", "image": "kiln/runner-ubuntu-24.04", "labels": ["vm"], "min_idle": 1, "max": 2 }))?;
    assert!(vm.to_string().contains("no runner connected now announces the firecracker executor"), "{vm}");
    assert_eq!(vm.pointer("/preview/heading_for"), Some(&json!(1)), "its minimum idle: {vm}");
    let mine = pool(json!({ "name": "mine", "executor": "docker", "image": "kiln/runner-ubuntu-24.04", "labels": ["local"], "max": 1 }))?;
    assert!(mine.to_string().contains("carries all of local itself"), "{mine}");

    // An environment: a reviewer who isn't anyone here, alone and not allowed their own; a
    // branches rule that leaves the trunk out.
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/env", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let env = |name: &str, protection: Value| -> anyhow::Result<Value> {
        let (st, body) = http(s.port, "POST", "/api/ops/ci.environments.save", &s.admin, Some(&json!({ "repo": "acme/env", "name": name, "protection": protection })))?;
        anyhow::ensure!(st == 200, "{st} {body}");
        Ok(serde_json::from_str(&body)?)
    };
    let prod = env("prod", json!({ "reviewers": ["ghost"], "prevent-self-review": true }))?;
    let said = prod.to_string();
    assert!(said.contains("reviewer ghost isn't anyone here") && said.contains("the only reviewer"), "{prod}");
    assert!(prod.pointer("/preview/a_deployment_from_the_trunk").and_then(Value::as_str).is_some_and(|m| m.contains("a review by one of ghost")), "{prod}");
    let staging = env("staging", json!({ "branches": ["release/*"] }))?;
    assert!(staging.to_string().contains("may not deploy to staging"), "{staging}");
    Ok(())
}

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

/// A token for `body`'s principal (`/api/tokens`).
fn token(s: &Server, body: Value) -> anyhow::Result<String> {
    let (st, out) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&body))?;
    anyhow::ensure!((200..300).contains(&st), "token: {st} {out}");
    serde_json::from_str::<Value>(&out)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {out}"))
}

/// One MCP call (`tools/call`) with `token`: whether it failed, and what it said.
fn mcp(s: &Server, token: &str, tool: &str, arguments: Value) -> anyhow::Result<(bool, String)> {
    let (st, body) = http(s.port, "POST", "/api/mcp", token, Some(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": tool, "arguments": arguments } })))?;
    anyhow::ensure!(st == 200, "{tool}: {st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    Ok((v.pointer("/result/isError").and_then(Value::as_bool).unwrap_or(true), v.pointer("/result/content/0/text").and_then(Value::as_str).unwrap_or_default().to_owned()))
}

/// W20-AC03: an agent whose grant only reads can't rerun, through any surface: the route, the
/// operation over HTTP, its MCP tool, code mode and the CLI (`kiln rerun`, `kiln op`), each
/// refusing it for the capability it lacks. Nor can an agent acting for a viewer, though its
/// own grant is the owner's. What they read, they read; an agent with the grant reruns.
#[test]
fn w20_ac03_authority() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("work"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    // The trunk, and once kiln has read it, a failing workflow pushed to it.
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        if !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::write(work.join(".kiln/ci.yaml"), "on: push\njobs:\n  test:\n    runs-on: local\n    steps:\n      - run: exit 1\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "ci"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let deadline = Instant::now() + Duration::from_secs(240);
    let run = loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=5", &s.admin, None)?;
        let done = serde_json::from_str::<Value>(&body)?
            .pointer("/runs/0")
            .filter(|r| r.get("conclusion").and_then(Value::as_str) == Some("failure"))
            .and_then(|r| r.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(id) = done {
            break id;
        }
        anyhow::ensure!(Instant::now() < deadline, "no failed run: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(300));
    };
    for (name, grants) in [("dev", json!(["developer@repo:acme/app"])), ("vera", json!(["viewer"]))] {
        let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": grants })))?;
        anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    }
    // An agent whose grant reads, for a developer who may rerun; an agent with the owner's grant
    // acting for a viewer; and an agent with a developer's grant, for the developer.
    let reader = token(&s, json!({ "kind": "agent", "agent": "reader", "user": "dev", "grants": ["viewer@repo:acme/app"] }))?;
    let broad = token(&s, json!({ "kind": "agent", "agent": "broad", "user": "vera", "grants": ["owner"] }))?;
    let fixer = token(&s, json!({ "kind": "agent", "agent": "fixer", "user": "dev", "grants": ["developer@repo:acme/app"] }))?;
    let url = format!("http://127.0.0.1:{}", s.port);
    let cli = |token: &str, args: &[&str]| -> anyhow::Result<(bool, String)> {
        let o = common::kiln().args(args).args(["--url", &url, "--token", token]).env("HOME", dir.path()).output()?;
        Ok((o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))))
    };
    let program = "return await kiln.ci.runs.rerun({ run: input.run })";
    for (who, t) in [("an agent that only reads", &reader), ("an agent acting for a viewer", &broad)] {
        let (st, body) = http(s.port, "POST", &format!("/api/ci/runs/{run}/rerun"), t, Some(&json!({ "mode": "all" })))?;
        assert!(st == 403 && body.contains("runs.rerun"), "{who}, the route: {st} {body}");
        let (st, body) = http(s.port, "POST", "/api/ops/ci.runs.rerun", t, Some(&json!({ "run": run })))?;
        assert!(st == 403 && body.contains("runs.rerun"), "{who}, the operation: {st} {body}");
        let (failed, said) = mcp(&s, t, "ci_runs_rerun", json!({ "run": run }))?;
        assert!(failed && said.contains("runs.rerun"), "{who}, its tool: {said}");
        let (failed, said) = mcp(&s, t, "code_run", json!({ "program": program, "input": { "run": run } }))?;
        assert!(failed && said.contains("runs.rerun"), "{who}, code mode: {said}");
        let (ok, said) = cli(t, &["rerun", &run])?;
        assert!(!ok && said.contains("runs.rerun"), "{who}, kiln rerun: {said}");
        let (ok, said) = cli(t, &["op", "ci.runs.rerun", &format!("run={run}")])?;
        assert!(!ok && said.contains("runs.rerun"), "{who}, kiln op: {said}");
        // What it reads, it reads.
        assert_eq!(http(s.port, "GET", &format!("/api/ci/runs/{run}"), t, None)?.0, 200, "{who} reads the run");
        let (failed, said) = mcp(&s, t, "ci_runs_get", json!({ "run": run }))?;
        assert!(!failed && said.contains(&run), "{who} reads it by its tool: {said}");
    }
    let (st, body) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
    anyhow::ensure!(st == 200, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body)?.pointer("/run/attempt"), Some(&json!(1)), "nothing reran it");
    // With the grant, it reruns.
    let (failed, said) = mcp(&s, &fixer, "ci_runs_rerun", json!({ "run": run }))?;
    assert!(!failed && said.contains("\"attempt\":2"), "an agent that may: {said}");
    Ok(())
}

/// The commands docs/ci.md §4.3 lists, each as its words up to its first argument: `kiln secrets
/// set|list` is `secrets set` and `secrets list`; `kiln clone · submit` is `clone` and `submit`.
fn listed_commands(doc: &str) -> Vec<Vec<String>> {
    let start = doc.find("### 4.3 CLI").unwrap_or(0);
    let end = doc.get(start..).and_then(|d| d.find("### 4.4")).map_or(doc.len(), |e| start + e);
    let mut out: Vec<Vec<String>> = Vec::new();
    for line in doc.get(start..end).unwrap_or_default().lines().filter(|l| l.trim_start().starts_with("kiln ")) {
        // Two commands a line, two or more spaces apart; `·` between commands that share `kiln`.
        let mut commands: Vec<String> = Vec::new();
        for part in line.split("  ").map(str::trim).filter(|p| !p.is_empty()) {
            if part.starts_with("kiln ") {
                commands.extend(part.split(" · ").map(|c| c.trim_start_matches("kiln ").to_owned()));
            } else if let Some(last) = commands.last_mut() {
                // `(in jobs)` and the like belong to the one before.
                last.push(' ');
                last.push_str(part);
            }
        }
        for c in commands {
            let mut words: Vec<Vec<String>> = Vec::new();
            for w in c.split_whitespace() {
                let optional = w.starts_with('[') && !w.starts_with("[-");
                let w = w.trim_matches(|ch| ch == '[' || ch == ']');
                if w.is_empty() || w.starts_with(['-', '<', '(']) || w.chars().any(|ch| ch.is_ascii_uppercase() || ch == '…' || ch == '@') {
                    break;
                }
                let alternatives: Vec<String> = w.split('|').map(str::to_owned).collect();
                words.push(if optional { [vec![String::new()], alternatives].concat() } else { alternatives });
            }
            // Every combination of the alternatives, a `[…]` one with and without.
            let mut all: Vec<Vec<String>> = vec![Vec::new()];
            for alts in words {
                all = all.iter().flat_map(|prefix| alts.iter().map(move |a| if a.is_empty() { prefix.clone() } else { [prefix.clone(), vec![a.clone()]].concat() })).collect();
            }
            for a in all {
                if !a.is_empty() && !out.contains(&a) {
                    out.push(a);
                }
            }
        }
    }
    out
}

/// What §4.3 lists that isn't built yet: W24's commands, specified before they're built.
const CLI_LEFT: &[&str] = &["gocacheprog", "rustc", "work gc", "work reap", "work status", "work verify", "work warm", "work why"];

/// W20-AC01 for the CLI: every command docs/ci.md §4.3 lists is one of kiln's (its `--help`
/// works), what isn't built yet ([`CLI_LEFT`]) aside.
#[test]
fn w20_ac01_every_command_section_4_3_lists() -> anyhow::Result<()> {
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/ci.md"))?;
    let commands = listed_commands(&doc);
    anyhow::ensure!(commands.len() > 60, "§4.3 read: {commands:?}");
    let home = tempfile::tempdir()?;
    let mut missing: Vec<String> = Vec::new();
    for c in &commands {
        let o = common::kiln().args(c).arg("--help").env("HOME", home.path()).env_remove("KILN_URL").env_remove("KILN_ADMIN_TOKEN").output()?;
        if !o.status.success() {
            missing.push(c.join(" "));
        }
    }
    missing.sort();
    assert_eq!(missing, CLI_LEFT, "what §4.3 lists and kiln lacks");
    Ok(())
}

/// One MCP request with `token`: its `result`, or why it failed.
fn rpc(s: &Server, token: &str, method: &str, params: Value) -> anyhow::Result<Value> {
    let (st, body) = http(s.port, "POST", "/api/mcp", token, Some(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })))?;
    anyhow::ensure!(st == 200, "{method}: {st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    v.get("result").cloned().ok_or_else(|| anyhow::anyhow!("{method}: {body}"))
}

/// An operation over HTTP as the admin: its answer.
fn op(s: &Server, name: &str, input: Value) -> anyhow::Result<Value> {
    let (st, body) = http(s.port, "POST", &format!("/api/ops/{name}"), &s.admin, Some(&input))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    Ok(serde_json::from_str(&body).unwrap_or(Value::Null))
}

const REPORTED: &str = r#"on: [push, workflow_dispatch]
jobs:
  test:
    runs-on: local
    environment: production
    steps:
      - name: unit tests
        test-results: { path: report.xml }
        run: |
          printf '%s' '<?xml version="1.0"?><testsuite name="calc" tests="1"><testcase classname="calc" name="adds" time="0.2"/></testsuite>' > report.xml
          echo "tested the calculator"
"#;

/// W20-AC01's new surfaces, on a server with a run: §4.4's MCP resources (a run's logs, a step's
/// log, a workflow's file) as the caller reads them; a runner's facts probed on request
/// (W13-R01); the store verified, a corrupted chunk found and moved aside (W08-R01); a test's
/// history (W16-R03); and a deployment by hand, the workflow that deploys there dispatched.
#[test]
fn w20_ac01_resources_and_what_was_missing_underneath() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("work-tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let until = |secs: u64, what: &str, f: &mut dyn FnMut() -> anyhow::Result<Option<String>>| -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            match f()? {
                None => return Ok(()),
                Some(last) => anyhow::ensure!(Instant::now() < deadline, "{what}: {last}\n{}", s.log()),
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    };
    until(60, "the first poll", &mut || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::write(work.join(".kiln/ci.yaml"), REPORTED)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "ci"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let mut run = String::new();
    until(240, "the run", &mut || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=5", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body)?;
        run = v.pointer("/runs/0").filter(|r| r.get("conclusion").and_then(Value::as_str) == Some("success")).and_then(|r| r.get("id")).and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok(run.is_empty().then_some(body))
    })?;

    // The resources: their templates, then each read as the caller.
    let templates = rpc(&s, &s.admin, "resources/templates/list", json!({}))?;
    let uris: Vec<&str> = templates.get("resourceTemplates").and_then(Value::as_array).into_iter().flatten().filter_map(|t| t.get("uriTemplate").and_then(Value::as_str)).collect();
    assert_eq!(uris, ["kiln://runs/{run}/logs", "kiln://legs/{leg}/logs/{step}", "kiln://workflows/{owner}/{repo}/{path}"]);
    let read =
        |uri: &str| -> anyhow::Result<String> { Ok(rpc(&s, &s.admin, "resources/read", json!({ "uri": uri }))?.pointer("/contents/0/text").and_then(Value::as_str).unwrap_or_default().to_owned()) };
    let logs = read(&format!("kiln://runs/{run}/logs"))?;
    assert!(logs.contains("== test · unit tests ==") && logs.contains("tested the calculator"), "{logs}");
    let detail = op(&s, "ci.runs.get", json!({ "run": run }))?;
    let leg = detail.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let steps = op(&s, "ci.legs.get", json!({ "leg": leg }))?;
    let idx = steps
        .get("steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|st| st.get("name").and_then(Value::as_str) == Some("unit tests"))
        .and_then(|st| st.get("idx"))
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    assert!(read(&format!("kiln://legs/{leg}/logs/{idx}"))?.contains("tested the calculator"));
    assert_eq!(read("kiln://workflows/acme/app/.kiln/ci.yaml")?, REPORTED, "the file as the trunk has it");
    let viewer = {
        let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": "outsider", "grants": ["viewer@repo:acme/elsewhere"] })))?;
        anyhow::ensure!((200..300).contains(&st), "{body}");
        token(&s, json!({ "kind": "pat", "user": "outsider", "grants": ["viewer@repo:acme/elsewhere"] }))?
    };
    let (st, body) = http(s.port, "POST", "/api/mcp", &viewer, Some(&json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/read", "params": { "uri": format!("kiln://runs/{run}/logs") } })))?;
    assert!(st == 200 && body.contains("\"error\"") && !body.contains("tested the calculator"), "what the caller may not read, it doesn't: {body}");

    // A runner's facts, probed again now.
    let (_, body) = http(s.port, "GET", "/api/agents", &s.admin, None)?;
    let runner = serde_json::from_str::<Value>(&body)?.pointer("/0/name").and_then(Value::as_str).unwrap_or_default().to_owned();
    let before = serde_json::from_str::<Value>(&body)?.pointer("/0/facts/probed_at").and_then(Value::as_i64).unwrap_or_default();
    std::thread::sleep(Duration::from_millis(1100));
    let probed = op(&s, "ci.runners.probe", json!({ "runner": runner }))?;
    assert!(probed.pointer("/facts/probed_at").and_then(Value::as_i64).is_some_and(|t| t > before) && probed.pointer("/facts/os").is_some(), "{probed}");

    // The test's history: this run's result.
    let tests = op(&s, "ci.tests.repo", json!({ "repo": "acme/app" }))?;
    let test = tests.pointer("/tests/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let history = op(&s, "ci.tests.history", json!({ "repo": "acme/app", "test": test }))?;
    assert!(
        history.pointer("/test/name") == Some(&json!("adds")) && history.pointer("/results/0/run") == Some(&json!(run)) && history.pointer("/results/0/outcome") == Some(&json!("passed")),
        "{history}"
    );

    // The store: verified whole, then a chunk spoiled and found.
    let clean = op(&s, "ci.store.verify", json!({}))?;
    assert!(clean.get("chunks").and_then(Value::as_u64).is_some_and(|n| n >= 1) && clean.get("corrupt") == Some(&json!([])), "{clean}");
    let chunk = walk(&dir.path().join("data/cas/chunks")).into_iter().next().ok_or_else(|| anyhow::anyhow!("no chunk on disk"))?;
    std::fs::write(&chunk, b"not what it was")?;
    let url = format!("http://127.0.0.1:{}", s.port);
    let o = common::kiln().args(["store", "verify", "--url", &url, "--token", &s.admin]).env("HOME", dir.path()).output()?;
    let said = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    let name = chunk.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    assert!(!o.status.success() && said.contains(&format!("corrupt: {name}")), "kiln store verify: {said}");
    assert!(!chunk.exists() && dir.path().join("data/cas/corrupt").join(&name).exists(), "moved aside");

    // A deployment by hand: the workflow that deploys to production, dispatched.
    let deploy = op(&s, "ci.environments.deploy", json!({ "repo": "acme/app", "name": "production" }))?;
    assert_eq!(deploy.get("workflow"), Some(&json!(".kiln/ci.yaml")), "{deploy}");
    until(120, "the deploying run", &mut || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=5", &s.admin, None)?;
        Ok((!body.contains("\"event\":\"workflow_dispatch\"")).then_some(body))
    })?;
    let none = http(s.port, "POST", "/api/ops/ci.environments.deploy", &s.admin, Some(&json!({ "repo": "acme/app", "name": "staging" })))?;
    assert!(none.0 == 409 && none.1.contains("no workflow at main deploys to staging"), "{none:?}");
    Ok(())
}

/// Every file under `dir`.
fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}
