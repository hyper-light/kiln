//! W24-R23 (docs/ci.md): `kiln rerun --cold` runs again with no reuse, no warm state and no cache
//! restored. A workflow's `keep` job keeps a v1 `cache:` folder and its `memo` job opts into reuse.
//! The second push's run finds the folder the first kept and reuses the first's `memo`; a cold rerun
//! of it starts with the folder empty, saying why, and runs `memo` again.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, Server, http, start};
use serde_json::{Value, json};

fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let o = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    Ok(())
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

const WORKFLOW: &str = r#"on: push
jobs:
  keep:
    runs-on: local
    checkout: false
    cache: [kept]
    steps:
      - run: |
          echo "saw=$(cat kept/f 2>/dev/null || echo none)"
          echo "run-${{ github.run_number }}" > kept/f
  memo:
    runs-on: local
    reuse: { paths: [src] }
    steps:
      - run: echo "built as $(date +%s%N)"
"#;

/// Run `run`'s legs in `attempt` (the newest when `None`), once all have finished: each by its
/// key, with its detail and its logs.
fn legs(s: &Server, run: &str, attempt: Option<i64>) -> anyhow::Result<Vec<(String, Value, String)>> {
    let q = attempt.map(|a| format!("?attempt={a}")).unwrap_or_default();
    let mut found = Vec::new();
    until(s, 300, &format!("run {run}'s legs to finish"), || {
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}{q}"), &s.admin, None)?;
        found = serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok((found.len() < 2 || found.iter().any(|l| l.get("finished_at").is_none_or(Value::is_null))).then_some(detail))
    })?;
    let mut out = Vec::new();
    for leg in found {
        let id = leg.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{id}"), &s.admin, None)?;
        let mut log = String::new();
        for step in serde_json::from_str::<Value>(&about)?.get("steps").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.get("idx").and_then(Value::as_u64)) {
            let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{id}/logs?step={step}"), &s.admin, None)?;
            log.push_str(&text);
        }
        out.push((leg.get("key").and_then(Value::as_str).unwrap_or_default().to_owned(), leg, log));
    }
    Ok(out)
}

/// The newest run, once there are `count`.
fn newest(s: &Server, count: usize) -> anyhow::Result<String> {
    let mut id = String::new();
    until(s, 180, &format!("run {count}"), || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/cold&limit=20", &s.admin, None)?;
        let runs = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        id = runs.first().filter(|_| runs.len() >= count).and_then(|r| r.get("id")).and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok(id.is_empty().then_some(body))
    })?;
    Ok(id)
}

fn of<'a>(legs: &'a [(String, Value, String)], key: &str) -> anyhow::Result<&'a (String, Value, String)> {
    legs.iter().find(|(k, _, _)| k == key).ok_or_else(|| anyhow::anyhow!("no `{key}` leg: {legs:?}"))
}

/// W24-R23: a cold rerun reuses nothing, starts from nothing kept, and says so.
#[test]
fn w24_r23_a_cold_rerun_reuses_nothing_and_starts_from_nothing_kept() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: "" })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/cold", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join("src"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/cold.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "cold\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    let commit = |file: &str, text: &str, message: &str| -> anyhow::Result<()> {
        std::fs::write(work.join(file), text)?;
        git(&home, &work, &["add", "."])?;
        git(&home, &work, &["commit", "-qm", message])?;
        git(&home, &work, &["push", "-q", &push, "HEAD:main"])
    };
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join("src/version"), "1\n")?;
    commit(".kiln/cold.yaml", WORKFLOW, "the workflow")?;
    let first = newest(&s, 1)?;
    let one = legs(&s, &first, None)?;
    anyhow::ensure!(one.iter().all(|(_, l, _)| l.get("conclusion") == Some(&json!("success"))), "{one:?}\n{}", s.log());
    anyhow::ensure!(of(&one, "keep")?.2.contains("saw=none"), "the first keep leg starts empty");
    // A README-only push: keep finds what the first kept, and memo takes the first's result.
    commit("README.md", "cold, again\n", "readme")?;
    let second = newest(&s, 2)?;
    let two = legs(&s, &second, None)?;
    anyhow::ensure!(of(&two, "keep")?.2.contains("saw=run-1"), "the kept folder: {two:?}");
    anyhow::ensure!(of(&two, "memo")?.1.get("reused_from").is_some_and(|r| !r.is_null()), "memo reused: {two:?}");
    // The second run again, cold.
    let (st, body) = http(s.port, "POST", &format!("/api/ci/runs/{second}/rerun"), &s.admin, Some(&json!({ "mode": "all", "cold": true })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let attempt = serde_json::from_str::<Value>(&body)?.get("attempt").and_then(Value::as_i64).ok_or_else(|| anyhow::anyhow!("no attempt: {body}"))?;
    anyhow::ensure!(attempt >= 2, "{body}");
    let cold = legs(&s, &second, Some(attempt))?;
    anyhow::ensure!(cold.iter().all(|(_, l, _)| l.get("conclusion") == Some(&json!("success"))), "{cold:?}\n{}", s.log());
    let keep = &of(&cold, "keep")?.2;
    anyhow::ensure!(keep.contains("saw=none") && keep.contains("a cold run starts with nothing kept"), "cold, nothing kept: {keep}");
    let (_, memo, log) = of(&cold, "memo")?;
    anyhow::ensure!(memo.get("reused_from").is_none_or(Value::is_null) && log.contains("built as "), "cold, nothing reused: {memo}\n{log}");
    Ok(())
}
