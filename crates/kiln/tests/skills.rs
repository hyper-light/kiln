//! W20-AC05 (docs/ci.md): the CI skills' programs, run as an agent following them would run them
//! (`code_run` over MCP), on fixtures: a repository whose tests fail, its run, and a diff. Each
//! program must run as written and find what its skill says it finds, so a skill can't drift from
//! the operations it teaches.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, Server, http, start};
use serde_json::{Value, json};

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

/// A skill's JavaScript blocks, in order.
fn programs(skill: &str) -> anyhow::Result<Vec<String>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/skills").join(skill).join("SKILL.md");
    let text = std::fs::read_to_string(&path)?;
    let mut out = Vec::new();
    let mut rest = text.as_str();
    while let Some(at) = rest.find("```js\n") {
        let body = rest.get(at + 6..).unwrap_or_default();
        let end = body.find("\n```").ok_or_else(|| anyhow::anyhow!("{skill}: a ```js block isn't closed"))?;
        out.push(body.get(..end).unwrap_or_default().to_owned());
        rest = body.get(end + 4..).unwrap_or_default();
    }
    Ok(out)
}

/// Follows a skill: runs its `n`th program with `input`, and gives what it returned, or why it failed.
fn follow(s: &Server, skill: &str, n: usize, input: Value) -> anyhow::Result<Value> {
    let program = programs(skill)?.into_iter().nth(n).ok_or_else(|| anyhow::anyhow!("{skill} has no program {n}"))?;
    let (st, body) = http(s.port, "POST", "/api/mcp", &s.admin, Some(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "code_run", "arguments": { "program": program, "input": input } } })))?;
    anyhow::ensure!(st == 200, "{skill} #{n}: {st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    let text = v.pointer("/result/content/0/text").and_then(Value::as_str).unwrap_or_default().to_owned();
    anyhow::ensure!(!v.pointer("/result/isError").and_then(Value::as_bool).unwrap_or(true), "{skill} #{n} failed: {text}");
    // What the program returned, beside the calls it made.
    let answer: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    Ok(answer.get("result").cloned().unwrap_or(answer))
}

/// The fixture: `acme/skills`, whose `test` job fails (one of its two tests), and its runs.
const WORKFLOW: &str = r#"on: [push, diff]
jobs:
  build:
    runs-on: local
    steps:
      - name: compile
        run: echo compiled
  test:
    runs-on: local
    needs: [build]
    steps:
      - name: unit tests
        test-results: { path: report.xml }
        run: |
          printf '%s' '<?xml version="1.0"?><testsuite name="calc" tests="2" failures="1"><testcase classname="calc" name="adds" time="0.2"/><testcase classname="calc" name="divides" time="0.9"><failure message="boom">error: division by zero</failure></testcase></testsuite>' > report.xml
          echo "running 2 tests"
          echo "error: division by zero"
          exit 1
"#;

#[test]
fn w20_ac05_the_ci_skills_programs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/skills", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/skills.git", s.admin, s.port);
    std::fs::write(work.join("README.md"), "skills\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &url, "HEAD:main"])?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".kiln/ci.yaml"), WORKFLOW)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "ci"])?;
    git(&home, &work, &["push", "-q", &url, "HEAD:main"])?;
    let mut run = Value::Null;
    until(240, "the failing run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/skills&limit=5", &s.admin, None)?;
        run = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).and_then(|r| r.iter().find(|r| r.get("conclusion").and_then(Value::as_str) == Some("failure")).cloned()).unwrap_or(Value::Null);
        Ok(run.is_null().then(|| format!("{body}\n{}", s.log())))
    })?;
    let run_id = run.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run_id}"), &s.admin, None)?;
    let leg = serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).and_then(|l| l.iter().find(|l| l.get("key").and_then(Value::as_str) == Some("test")).cloned()).unwrap_or_default();
    let leg_id = leg.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    // A leg that waits, for the routing programs: no runner carries `nowhere`.
    std::fs::write(work.join(".kiln/waits.yaml"), "on: push\njobs:\n  stuck:\n    runs-on: [nowhere]\n    steps:\n      - run: echo never\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "waits"])?;
    git(&home, &work, &["push", "-q", &url, "HEAD:main"])?;
    let mut waiting = String::new();
    until(120, "the waiting leg", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/skills&limit=10", &s.admin, None)?;
        let runs: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        if let Some(r) = runs.iter().find(|r| r.get("workflow").and_then(Value::as_str).is_some_and(|w| w.ends_with("waits.yaml"))) {
            let (_, d) = http(s.port, "GET", &format!("/api/ci/runs/{}", r.get("id").and_then(Value::as_str).unwrap_or_default()), &s.admin, None)?;
            waiting = serde_json::from_str::<Value>(&d)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        }
        Ok(waiting.is_empty().then_some(body))
    })?;
    // A diff, for the review programs.
    git(&home, &work, &["checkout", "-qb", "fix"])?;
    std::fs::write(work.join("README.md"), "skills, fixed\n")?;
    git(&home, &work, &["commit", "-qam", &format!("Say it's fixed\n\nChange-Id: I{}", "5".repeat(40))])?;
    git(&home, &work, &["push", "-q", &url, "HEAD:refs/for/main"])?;

    // kiln-ci-diagnose: the failure bundle names the failing step and its error.
    let bundle = follow(&s, "kiln-ci-diagnose", 0, json!({ "repo": "acme/skills", "limit": 5 }))?;
    let first = bundle.get(0).cloned().unwrap_or_default();
    assert!(first.get("leg").and_then(Value::as_str) == Some("test") && first.get("step").and_then(Value::as_str) == Some("unit tests"), "{bundle}");
    assert!(first.get("excerpt").and_then(Value::as_str).is_some_and(|e| e.contains("division by zero")), "{bundle}");

    // kiln-ci-author: the loop, one program: checked, previewed, proposed.
    let fixed = WORKFLOW.replace("          exit 1\n", "");
    let authored = follow(&s, "kiln-ci-author", 0, json!({ "repo": "acme/skills", "path": ".kiln/ci.yaml", "content": fixed, "title": "Let the tests pass" }))?;
    assert!(authored.get("legs").and_then(Value::as_array).is_some_and(|l| l.iter().any(|x| x.get("key").and_then(Value::as_str) == Some("test"))), "{authored}");
    assert!(authored.pointer("/diff").is_some_and(|d| d.to_string().contains("D2")), "the change is a diff: {authored}");

    // kiln-ci-runners: why a leg waits, and where a job would go.
    let routed = follow(&s, "kiln-ci-runners", 0, json!({ "leg": waiting }))?;
    assert!(routed.to_string().contains("local") && routed.to_string().contains("nowhere"), "the runner, without the label: {routed}");
    let done = follow(&s, "kiln-ci-runners", 0, json!({ "leg": leg_id }))?;
    assert_eq!(done.get("known"), Some(&json!(false)), "a finished leg's routing is history: {done}");
    let would = follow(&s, "kiln-ci-runners", 1, json!({ "repo": "acme/skills" }))?;
    assert!(would.get("runners").and_then(Value::as_array).is_some_and(|r| !r.is_empty()), "{would}");

    // kiln-ci-review: what keeps the diff from landing, and its open threads.
    let ready = follow(&s, "kiln-ci-review", 1, json!({ "diff": "D1" }))?;
    assert!(ready.get("blockers").and_then(Value::as_array).is_some() && ready.get("state").is_some(), "{ready}");
    let open = follow(&s, "kiln-ci-review", 2, json!({ "diff": "D1", "fixed": [], "version": 1, "how": "" }))?;
    assert_eq!(open, json!(0), "no threads yet");

    // kiln-ci-secrets: names and last use, never values.
    let (st, body) = http(s.port, "PUT", "/api/ci/secrets/DEPLOY_KEY?scope=repo:acme/skills", &s.admin, Some(&json!({ "value": "never-shown-73" })))?;
    anyhow::ensure!((200..300).contains(&st), "secret: {st} {body}");
    let names = follow(&s, "kiln-ci-secrets", 0, json!({ "repo": "acme/skills" }))?;
    assert!(names.to_string().contains("DEPLOY_KEY") && !names.to_string().contains("never-shown-73"), "{names}");

    // kiln-ci-performance: where the time goes, and the slowest tests.
    let time = follow(&s, "kiln-ci-performance", 0, json!({ "repo": "acme/skills" }))?;
    assert!(time.get("workflow").and_then(Value::as_str) == Some(".kiln/ci.yaml") && time.get("critical_path").is_some(), "{time}");
    let slow = follow(&s, "kiln-ci-performance", 1, json!({ "repo": "acme/skills" }))?;
    assert_eq!(slow.pointer("/0/test").and_then(Value::as_str), Some("divides"), "the slowest first: {slow}");
    Ok(())
}
