//! W05-R03 (docs/ci.md): capacity in resource mode. With `[ci.scheduler] mode = "resources"`, a
//! runner offers what its facts say it has (CPUs, memory, disk, GPUs, each times its overcommit),
//! not its slots, and a leg takes what its job's `resources:` asks.

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

/// W05-R03: two legs each asking more than half of what this machine's CPUs offer (its threads,
/// times the default overcommit of 1.5) run one after the other on a runner with two slots, where
/// in slot mode they'd run at once; a third asking for a little runs beside one of them.
#[test]
fn w05_r03_legs_fit_the_runners_resources() -> anyhow::Result<()> {
    let threads = std::thread::available_parallelism()?.get();
    if threads < 4 {
        eprintln!("skipped: {threads} CPUs, too few to tell half of them from a little");
        return Ok(());
    }
    // More than half of 1.5 × threads, so two never fit at once.
    let big = threads * 3 / 4 + 1;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: "[ci.scheduler]\nmode = \"resources\"\n" })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/fit", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/fit.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "fit\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    // Facts first (`requires`), so the legs are placed by them.
    let step = "      - run: echo \"start $(date +%s%N)\"; sleep 4; echo \"end $(date +%s%N)\"\n";
    let workflow = format!(
        "on: push\njobs:\n  big:\n    runs-on: local\n    requires: [\"cpu.threads >= 1\"]\n    matrix: {{ n: [1, 2] }}\n    resources: {{ cpu: {big} }}\n    steps:\n{step}  small:\n    runs-on: local\n    requires: [\"cpu.threads >= 1\"]\n    resources: {{ cpu: 1 }}\n    steps:\n{step}"
    );
    std::fs::write(work.join(".kiln/fit.yaml"), workflow)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "fit"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let mut run = Value::Null;
    until(&s, 300, "the run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/fit&limit=5", &s.admin, None)?;
        run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0").cloned().unwrap_or_default();
        Ok(run.get("finished_at").is_none_or(Value::is_null).then_some(body))
    })?;
    assert_eq!(run.get("conclusion"), Some(&json!("success")), "{run}\n{}", s.log());
    // Each leg's start and end, as its step said them.
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let mut spans: Vec<(String, u128, u128)> = Vec::new();
    for l in serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
        let (leg, key) = (l.get("id").and_then(Value::as_str).unwrap_or_default(), l.get("key").and_then(Value::as_str).unwrap_or_default().to_owned());
        let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
        let mut log = String::new();
        for x in serde_json::from_str::<Value>(&about)?.get("steps").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.get("idx").and_then(Value::as_u64)) {
            log.push_str(&http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={x}"), &s.admin, None)?.1);
        }
        let at = |what: &str| log.lines().find_map(|l| l.split_once(what).and_then(|(_, t)| t.trim().parse::<u128>().ok()));
        let (Some(from), Some(to)) = (at("start "), at("end ")) else { anyhow::bail!("{key}'s times: {log}") };
        spans.push((key, from, to));
    }
    let overlap = |a: &str, b: &str| -> anyhow::Result<bool> {
        let find = |k: &str| spans.iter().find(|(key, _, _)| key.starts_with(k)).cloned().ok_or_else(|| anyhow::anyhow!("no leg {k}: {spans:?}"));
        let ((_, a0, a1), (_, b0, b1)) = (find(a)?, find(b)?);
        Ok(a0 < b1 && b0 < a1)
    };
    if cfg!(target_os = "linux") {
        assert!(!overlap("big (1)", "big (2)")?, "the two big legs, one after the other: {spans:?}");
        assert!(overlap("small", "big (1)")? || overlap("small", "big (2)")?, "the small one beside a big one: {spans:?}");
    }
    Ok(())
}
