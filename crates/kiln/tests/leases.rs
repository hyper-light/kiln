//! W05-R34 (docs/ci.md, docs/needs.md §8): a grant is a lease its runner renews. A leg that runs
//! past two of the server's leases keeps its grant and ends on its first attempt: renewed by a lease
//! runner's `Renew`, what it sends fenced by its grant; and, built with `chaos`, by the heartbeat of
//! a runner from before leases, which says nothing of them.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

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

/// A server whose grants live 20 s, with its own runner (`env` its faults), runs one leg of `secs`
/// seconds: the leg as the server has it once its run ended, with its attempts and its log.
fn outlive(dir: &Path, env: &[(&str, &str)], secs: u32) -> anyhow::Result<(Server, Value, String)> {
    let s = start_env(dir, &Options { agent: true, ci: "kiln_workflows = true", extra: "[sched]\nlease = \"20s\"\n" }, false, env)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/lease", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.join("githome"), dir.join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/lease.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "lease\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    let workflow = format!("on: push\njobs:\n  long:\n    runs-on: local\n    steps:\n      - run: echo began; sleep {secs}; echo ended\n");
    std::fs::write(work.join(".kiln/lease.yaml"), workflow)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "a leg past two leases"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let mut run = Value::Null;
    until(&s, u64::from(secs) + 240, "the run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/lease&limit=5", &s.admin, None)?;
        run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0").cloned().unwrap_or_default();
        Ok(run.get("finished_at").is_none_or(Value::is_null).then_some(body))
    })?;
    anyhow::ensure!(run.get("conclusion") == Some(&json!("success")), "the run: {run}\n{}", s.log());
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
    let about: Value = serde_json::from_str(&about)?;
    let mut log = String::new();
    for x in about.get("steps").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.get("idx").and_then(Value::as_u64)) {
        log.push_str(&http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={x}"), &s.admin, None)?.1);
    }
    Ok((s, about, log))
}

/// Its grant was never lost: one attempt, ended in success, its whole output taken, and nothing in
/// the server's log of a lease run out.
fn kept(s: &Server, about: &Value, log: &str) {
    let attempts = about.get("attempts").and_then(Value::as_array).cloned().unwrap_or_default();
    assert_eq!(attempts.len(), 1, "one attempt: {attempts:?}\n{}", s.log());
    assert_eq!(attempts.first().and_then(|a| a.get("conclusion")), Some(&json!("success")), "{attempts:?}");
    assert!(log.contains("began") && log.contains("ended"), "its output, whole: {log}");
    assert!(!s.log().contains("its lease ran out"), "a grant was lost: {}", s.log());
}

/// W05-R34: a lease runner renews its leg's grant every 10 s (`Renew`), past two leases of 20 s,
/// and what it sends about the leg (fenced by its grant) is taken.
#[test]
fn w05_r34_a_lease_runner_keeps_its_grant_past_two_leases() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (s, about, log) = outlive(dir.path(), &[], 45)?;
    kept(&s, &about, &log);
    Ok(())
}

/// W05-R34: a runner from before leases (`old`: it says nothing past `Build`'s 32 bits) keeps its
/// leg's grant by its heartbeat, which names the legs it runs.
#[cfg(feature = "chaos")]
#[test]
fn w05_r34_an_older_runners_heartbeat_keeps_its_grant() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (s, about, log) = outlive(dir.path(), &[("KILN_CHAOS", "old")], 45)?;
    kept(&s, &about, &log);
    Ok(())
}
