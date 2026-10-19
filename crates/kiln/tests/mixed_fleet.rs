//! A mixed fleet (docs/ci.md W22-AC09): a server at this version, its own runner at this version,
//! and a runner built from `main` before the CI work (`KILN_OLD_BIN`, a kiln of edc9ae8: no
//! `CAP_CI2`). v1 jobs keep running on the old runner; a CI engine leg only the old runner could
//! take waits with `capability`, naming it; one the new runner can take runs there; nothing fails
//! because of the mix.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{Options, start};
use serde_json::Value;

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let o = Command::new("git").arg("-C").arg(dir).args(args).env("GIT_AUTHOR_NAME", "T").env("GIT_AUTHOR_EMAIL", "t@example.com").env("GIT_COMMITTER_NAME", "T").env("GIT_COMMITTER_EMAIL", "t@example.com").output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(())
}

/// The old runner, killed when the test ends.
struct Old(Child);

impl Drop for Old {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn get(port: u16, admin: &str, path: &str) -> anyhow::Result<Value> {
    let (_, body) = common::http(port, "GET", path, admin, None)?;
    Ok(serde_json::from_str(&body).unwrap_or_default())
}

#[test]
fn w22_ac09_mixed_fleet() -> anyhow::Result<()> {
    let Some(old) = std::env::var_os("KILN_OLD_BIN") else {
        eprintln!("w22_ac09_mixed_fleet: set KILN_OLD_BIN to a kiln built from main before the CI work (edc9ae8)");
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::create_dir_all(repo.join(".github/workflows"))?;
    // v1's pipeline, for the old runner; the engine's workflow: one leg only the old runner's
    // labels match, one the new runner's.
    std::fs::write(repo.join(".kiln/pipeline.yaml"), "name: v1\non:\n  push:\n    branches: [main]\njobs:\n  on-old:\n    runs-on: [old]\n    steps:\n      - run: echo \"v1 ran here\"\n")?;
    std::fs::write(
        repo.join(".github/workflows/v2.yml"),
        "name: v2\non: push\njobs:\n  on-new:\n    runs-on: [self-hosted, local]\n    steps:\n      - run: echo \"v2 ran here\"\n  old-only:\n    runs-on: [self-hosted, old]\n    steps:\n      - run: echo never\n",
    )?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!("[[repos]]\nname = \"acme/mixed\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    // GitHub's workflows run beside `.kiln/` pipelines (v1 keeps those).
    let s = start(dir.path(), &Options { agent: true, ci: "github_workflows = \"on\"", extra: &extra })?;
    let (port, admin) = (s.port, s.admin.clone());
    let join = std::fs::read_to_string(dir.path().join("data/join.token"))?.trim().to_owned();
    // The old runner joins as runners did then.
    let home = dir.path().join("old-home");
    std::fs::create_dir_all(&home)?;
    let log = std::fs::File::create(dir.path().join("old-agent.log"))?;
    let child = Command::new(&old)
        .args(["agent", "--server", &format!("http://127.0.0.1:{port}"), "--token", &join, "--name", "old-runner", "--labels", "old", "--no-terminal"])
        .arg("--work-dir")
        .arg(dir.path().join("old-work"))
        .arg("--state-dir")
        .arg(dir.path().join("old-state"))
        .env("HOME", &home)
        .env("NO_COLOR", "1")
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?;
    let _old = Old(child);
    let old_log = || std::fs::read_to_string(dir.path().join("old-agent.log")).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let agents = get(port, &admin, "/api/agents")?;
        let list = agents.as_array().cloned().or_else(|| agents.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
        if list.iter().any(|a| a.get("name").and_then(Value::as_str) == Some("old-runner") && a.get("connected").and_then(Value::as_bool).unwrap_or(true)) {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the old runner never connected: {agents}\n{}\n{}", old_log(), s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
    // The repository's first poll takes its commits as they are; the push after it runs.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (_, body) = common::http(port, "GET", "/api/repos", &admin, None)?;
        if body.contains("acme/mixed") && !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::write(repo.join("README.md"), "mixed\n")?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "go"])?;
    // v1: its job on the old runner, done.
    let deadline = Instant::now() + Duration::from_secs(300);
    let v1 = loop {
        let runs = get(port, &admin, "/api/runs?repo=acme/mixed")?;
        let list = runs.as_array().cloned().or_else(|| runs.get("runs").and_then(Value::as_array).cloned()).unwrap_or_default();
        let mut found = None;
        for r in &list {
            let id = r.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let detail = get(port, &admin, &format!("/api/runs/{id}"))?;
            if let Some(job) = detail.get("jobs").and_then(Value::as_array).and_then(|j| j.iter().find(|j| j.get("key").and_then(Value::as_str) == Some("on-old"))) {
                found = Some(job.clone());
            }
        }
        if let Some(job) = found.filter(|j| j.get("status").and_then(Value::as_str).is_some_and(|st| matches!(st, "succeeded" | "failed" | "cancelled" | "timed_out"))) {
            break job;
        }
        anyhow::ensure!(Instant::now() < deadline, "the v1 job didn't finish: {runs}\n--- old runner:\n{}\n--- server:\n{}", old_log(), s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    assert_eq!(v1.get("status").and_then(Value::as_str), Some("succeeded"), "{v1}\n{}", old_log());
    assert_eq!(v1.get("agent").and_then(Value::as_str), Some("old-runner"), "{v1}");
    // The engine: on-new ran on the new runner; old-only waits for a newer kiln on the old one.
    let deadline = Instant::now() + Duration::from_secs(300);
    let (on_new, old_only) = loop {
        let runs = get(port, &admin, "/api/ci/runs?repo=acme/mixed&limit=5")?;
        if let Some(id) = runs.get("runs").and_then(Value::as_array).and_then(|r| r.first()).and_then(|r| r.get("id")).and_then(Value::as_str) {
            let detail = get(port, &admin, &format!("/api/ci/runs/{id}"))?;
            let legs = detail.get("legs").and_then(Value::as_array).cloned().unwrap_or_default();
            let leg = |k: &str| legs.iter().find(|l| l.get("key").and_then(Value::as_str) == Some(k)).cloned();
            if let (Some(a), Some(b)) = (leg("on-new"), leg("old-only"))
                && a.get("conclusion").and_then(Value::as_str).is_some()
                && b.get("wait_reason").is_some_and(|w| !w.is_null())
            {
                break (a, b);
            }
        }
        anyhow::ensure!(Instant::now() < deadline, "the engine's legs didn't settle: {runs}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    assert_eq!(on_new.get("conclusion").and_then(Value::as_str), Some("success"), "{on_new}");
    assert_ne!(on_new.get("runner").and_then(Value::as_str), Some("old-runner"), "{on_new}");
    let why = old_only.get("wait_reason").cloned().unwrap_or_default();
    assert_eq!(why.get("reason").and_then(Value::as_str), Some("capability"), "{old_only}");
    assert!(why.get("runners").and_then(Value::as_array).is_some_and(|r| r.iter().any(|n| n.as_str() == Some("old-runner"))), "it names the old runner: {why}");
    assert!(old_only.get("conclusion").is_none_or(Value::is_null), "it waits, never fails: {old_only}");
    Ok(())
}
