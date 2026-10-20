//! Bisecting a failure (docs/ci.md §4.1's `ci.runs.bisect`, W20-R09's `diagnose`): a failed run's
//! job run again on the commits since it last passed, the range halved each time, until the first
//! commit it fails on is found.

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
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn op(s: &Server, name: &str, input: Value) -> anyhow::Result<(u16, Value)> {
    let (st, body) = http(s.port, "POST", &format!("/api/ops/{name}"), &s.admin, Some(&input))?;
    Ok((st, serde_json::from_str(&body).unwrap_or(Value::String(body))))
}

/// The last run of the repository, once it has ended: its id and conclusion.
fn last_run(s: &Server, number: i64) -> anyhow::Result<(String, String)> {
    let mut found = (String::new(), String::new());
    until(s, 240, &format!("run #{number}"), || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=10", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body)?;
        let run = v.get("runs").and_then(Value::as_array).and_then(|r| r.iter().find(|r| r.get("number").and_then(Value::as_i64) == Some(number)).cloned());
        if let Some(c) = run.as_ref().and_then(|r| r.get("conclusion")).and_then(Value::as_str) {
            found = (run.as_ref().and_then(|r| r.get("id")).and_then(Value::as_str).unwrap_or_default().to_owned(), c.to_owned());
            return Ok(None);
        }
        Ok(Some(body))
    })?;
    Ok(found)
}

const CI: &str = "on: push\njobs:\n  test:\n    runs-on: local\n    steps:\n      - run: test ! -e bug\n  lint:\n    runs-on: local\n    steps:\n      - run: echo linted\n";

/// Of four commits after the last green run, the second breaks the job: the bisection tries the
/// middle (broken), then the first (fine), and names the second, with its message and author.
/// The job alone runs each time, whatever the push filters say.
#[test]
fn w20_a_bisection_finds_the_commit_that_broke_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    let commit = |file: &str, text: &str, message: &str| -> anyhow::Result<String> {
        std::fs::write(work.join(file), text)?;
        git(&home, &work, &["add", "."])?;
        git(&home, &work, &["commit", "-qm", message])?;
        Ok(git(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned())
    };
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    commit("README.md", "app\n", "first")?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    let good = commit(".kiln/ci.yaml", CI, "ci")?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let (green, passed) = last_run(&s, 1)?;
    anyhow::ensure!(passed == "success", "run #1: {passed}");
    commit("one", "1\n", "one")?;
    let culprit = commit("bug", "here\n", "add the bug")?;
    commit("two", "2\n", "two")?;
    commit("three", "3\n", "three")?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let (red, failed) = last_run(&s, 2)?;
    anyhow::ensure!(failed == "failure", "run #2: {failed}");

    // Nothing failed in the green one; a `good` that isn't the run's history is refused.
    let (st, said) = op(&s, "ci.runs.bisect", json!({ "run": green }))?;
    assert!(st == 409 && said.to_string().contains("none of the run's jobs failed"), "{st} {said}");
    let (st, said) = op(&s, "ci.runs.bisect", json!({ "run": red, "good": "0123456789abcdef0123456789abcdef01234567" }))?;
    assert_eq!(st, 409, "{said}");

    // The failed job, between the last green run's commit and this one's.
    let (st, started) = op(&s, "ci.runs.bisect", json!({ "run": red }))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {started}");
    let id = started.pointer("/bisect/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    assert!(started.pointer("/bisect/job") == Some(&json!("test")) && started.pointer("/bisect/good") == Some(&json!(good)), "{started}");
    assert_eq!(started.pointer("/bisect/commits").and_then(Value::as_array).map(Vec::len), Some(4), "{started}");
    let mut done = Value::Null;
    until(&s, 300, "the bisection", || {
        let (_, v) = op(&s, "ci.runs.bisects", json!({ "run": red, "bisect": id }))?;
        done = v.get("bisect").cloned().unwrap_or_default();
        Ok((done.get("state").and_then(Value::as_str) == Some("running")).then(|| done.to_string()))
    })?;
    assert_eq!(done.get("state"), Some(&json!("found")), "{done}");
    assert_eq!(done.pointer("/culprit/sha"), Some(&json!(culprit)), "{done}");
    assert_eq!(done.pointer("/culprit/message").and_then(Value::as_str).map(str::trim), Some("add the bug"), "{done}");
    let tried: Vec<(String, String)> = done
        .get("tried")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|t| (t.get("sha").and_then(Value::as_str).unwrap_or_default().to_owned(), t.get("conclusion").and_then(Value::as_str).unwrap_or_default().to_owned()))
        .collect();
    assert_eq!(tried.len(), 2, "two tries for four commits: {done}");
    assert_eq!(tried.first().map(|t| (t.0.as_str(), t.1.as_str())), Some((culprit.as_str(), "failure")), "the middle first: {done}");
    assert_eq!(tried.get(1).map(|t| t.1.as_str()), Some("success"), "then the one before it: {done}");
    // Each try ran the job alone.
    let try_run = done.pointer("/tried/0/run").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{try_run}"), &s.admin, None)?;
    let jobs: Vec<String> =
        serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).into_iter().flatten().filter_map(|l| l.get("job").and_then(Value::as_str).map(str::to_owned)).collect();
    assert_eq!(jobs, ["test"], "{detail}");
    Ok(())
}
