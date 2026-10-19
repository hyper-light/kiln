//! A runner killed mid-leg leaves the leg's containers and network behind. The next time it
//! starts, it removes them, found by its own label, before it takes a leg, and leaves what
//! another runner on the same engine started.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, http, start};
use serde_json::Value;

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let o = Command::new("git").arg("-C").arg(dir).args(args).env("GIT_AUTHOR_NAME", "T").env("GIT_AUTHOR_EMAIL", "t@example.com").env("GIT_COMMITTER_NAME", "T").env("GIT_COMMITTER_EMAIL", "t@example.com").output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(())
}

fn docker(args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("docker").args(args).output()?;
    anyhow::ensure!(o.status.success(), "docker {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// Whether the engine still has `id` (a container or a network).
fn exists(kind: &str, id: &str) -> bool {
    Command::new("docker").args([kind, "inspect", id]).output().is_ok_and(|o| o.status.success())
}

const WORKFLOW: &str = "on: push\njobs:\n  long:\n    runs-on: local\n    container: alpine:3.20\n    steps:\n      - run: sleep 600\n";

#[test]
fn a_runner_removes_what_it_left_running() -> anyhow::Result<()> {
    if docker(&["info", "--format", "{{.ServerVersion}}"]).is_err() {
        eprintln!("a_runner_removes_what_it_left_running: skipped, no Docker");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo)?;
    std::fs::write(repo.join("README.md"), "long\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!("[[repos]]\nname = \"acme/long\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    let opts = Options { agent: true, extra: &extra, ..Options::default() };
    let s = start(dir.path(), &opts)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        if body.contains("acme/long") && !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::create_dir_all(repo.join(".github/workflows"))?;
    std::fs::write(repo.join(".github/workflows/long.yml"), WORKFLOW)?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "long"])?;
    // The leg, running: its job container up.
    let deadline = Instant::now() + Duration::from_secs(300);
    let leg = loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/long&limit=1", &s.admin, None)?;
        let runs: Value = serde_json::from_str(&body).unwrap_or_default();
        if let Some(id) = runs.pointer("/runs/0/id").and_then(Value::as_str) {
            let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
            let detail: Value = serde_json::from_str(&detail).unwrap_or_default();
            if let Some(leg) = detail.pointer("/legs/0/id").and_then(Value::as_str)
                && !docker(&["ps", "-q", "--filter", &format!("label=kiln.leg={leg}")])?.is_empty()
            {
                break leg.to_owned();
            }
        }
        anyhow::ensure!(Instant::now() < deadline, "the leg never started its containers: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let containers: Vec<String> = docker(&["ps", "-aq", "--filter", &format!("label=kiln.leg={leg}")])?.lines().map(str::to_owned).collect();
    let networks: Vec<String> = docker(&["network", "ls", "-q", "--filter", &format!("label=kiln.leg={leg}")])?.lines().map(str::to_owned).collect();
    // Another runner's network on the same engine: never this runner's to remove.
    let theirs = format!("kiln_leftovers_test_{}", std::process::id());
    let other = docker(&["network", "create", "--label", "kiln.runner-work=someone-else", "--label", "kiln.leg=theirs", &theirs])?;
    // Killed, as a crash would: nothing cleaned up.
    drop(s);
    std::thread::sleep(Duration::from_secs(2));
    assert!(containers.iter().all(|c| exists("container", c)), "a killed runner's containers stay: {containers:?}");
    // Started again on the same work directory: what it left goes before it takes a leg.
    let s = start(dir.path(), &opts)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while containers.iter().any(|c| exists("container", c)) || networks.iter().any(|n| exists("network", n)) {
        anyhow::ensure!(Instant::now() < deadline, "what the runner left is still there: {containers:?} {networks:?}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
    let kept = exists("network", &other);
    let _ = docker(&["network", "rm", &theirs]);
    // The server may have started the leg again: nothing of it outlives the test.
    drop(s);
    let again: Vec<String> = docker(&["ps", "-aq", "--filter", &format!("label=kiln.leg={leg}")])?.lines().map(str::to_owned).collect();
    if !again.is_empty() {
        let mut rm = vec!["rm", "--force"];
        rm.extend(again.iter().map(String::as_str));
        let _ = docker(&rm);
    }
    for n in docker(&["network", "ls", "-q", "--filter", &format!("label=kiln.leg={leg}")])?.lines() {
        let _ = docker(&["network", "rm", n]);
    }
    assert!(kept, "another runner's network was removed");
    Ok(())
}
