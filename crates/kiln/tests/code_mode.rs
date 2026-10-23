//! W20-AC02 (docs/ci.md): one code-mode program over a repository's last hundred runs: their
//! failed legs, grouped by failure class and counted, in under five seconds. Here against a local
//! server with a hundred runs; the AC's own check is the live server's, once it runs KilnCI.

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

fn until(s: &Server, secs: u64, what: &str, mut f: impl FnMut() -> anyhow::Result<Option<String>>) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match f()? {
            None => return Ok(()),
            Some(last) => anyhow::ensure!(Instant::now() < deadline, "{what}: {last}\n{}", s.log()),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Two jobs that fail, each its own way: a plain failure, and a full disk.
const FAILING: &str = r#"on: push
jobs:
  unit:
    runs-on: local
    steps:
      - run: echo "1 test failed"; exit 1
  space:
    runs-on: local
    steps:
      - run: |
          echo "write error: No space left on device" >&2
          exit 1
"#;

/// The program, as an agent would write it: the last hundred runs a page at a time, the failed
/// ones' legs, and each failed leg's latest attempt's class.
const PROGRAM: &str = r#"
const t0 = Date.now();
let runs = [], cursor;
while (runs.length < 100) {
  const page = await kiln.ci.runs.list({ repo: input.repo, limit: 100 - runs.length, ...(cursor && { cursor }) });
  runs.push(...page.runs);
  if (!page.next_cursor || page.runs.length === 0) break;
  cursor = page.next_cursor;
}
const failed = runs.filter((r) => r.conclusion && !["success", "skipped", "neutral"].includes(r.conclusion));
const legs = [];
let sample;
for (let i = 0; i < failed.length; i += 16) {
  const details = await Promise.all(failed.slice(i, i + 16).map((r) => kiln.ci.runs.get({ run: r.id })));
  sample = sample || details[0];
  for (const d of details) legs.push(...d.legs.filter((l) => l.conclusion === "failure" || l.conclusion === "timed_out"));
}
const by_class = {};
for (let i = 0; i < legs.length; i += 16) {
  const got = await Promise.all(legs.slice(i, i + 16).map((l) => kiln.ci.legs.get({ leg: l.id })));
  for (const g of got) {
    const last = (g.attempts || []).slice(-1)[0] || {};
    const c = last.class || "unclassified";
    by_class[c] = (by_class[c] || 0) + 1;
  }
}
return { runs: runs.length, failed_runs: failed.length, failed_legs: legs.length, by_class, ms: Date.now() - t0, sample: legs.length ? undefined : sample };
"#;

#[test]
fn w20_ac02_one_program_over_the_last_hundred_runs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::write(work.join(".kiln/ci.yaml"), FAILING)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "ci"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let sha = git(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();
    // A hundred runs in all: the push's, and 99 more of the same commit.
    until(&s, 120, "the push's run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=5", &s.admin, None)?;
        Ok((!body.contains("\"runs\":[{")).then_some(body))
    })?;
    for _ in 0..99 {
        let (st, body) = http(s.port, "POST", "/api/ops/ci.runs.local", &s.admin, Some(&json!({ "repo": "acme/app", "sha": sha, "ref": "main" })))?;
        anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    }
    until(&s, 900, "a hundred runs, ended", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=100", &s.admin, None)?;
        let runs: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        let ended = runs.iter().filter(|r| r.get("conclusion").is_some_and(|c| !c.is_null())).count();
        Ok((runs.len() < 100 || ended < 100).then(|| format!("{} runs, {ended} ended", runs.len())))
    })?;
    let (st, body) = http(
        s.port,
        "POST",
        "/api/mcp",
        &s.admin,
        Some(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "code_run", "arguments": { "program": PROGRAM, "input": { "repo": "acme/app" } } } })),
    )?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    let text = v.pointer("/result/content/0/text").and_then(Value::as_str).unwrap_or_default();
    anyhow::ensure!(v.pointer("/result/isError") == Some(&json!(false)), "{text}");
    let out: Value = serde_json::from_str::<Value>(text)?.get("result").cloned().unwrap_or_default();
    assert_eq!(out.get("runs"), Some(&json!(100)), "{out}");
    assert_eq!(out.get("failed_legs"), Some(&json!(200)), "{out}");
    assert_eq!(out.get("by_class"), Some(&json!({ "disk-full": 100, "failure": 100 })), "{out}");
    let ms = out.get("ms").and_then(Value::as_u64).unwrap_or(u64::MAX);
    assert!(ms < 5000, "in under five seconds: {ms} ms");
    Ok(())
}
