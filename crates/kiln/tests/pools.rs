//! Pools on a real runner (docs/ci.md W13-R09, W13-AC04): a `docker` pool's executors started on
//! the server's own runner, legs run in them and each discarded, the pool scaled to `max` by a
//! burst and back to `min_idle` within `idle_timeout`.

#![cfg(unix)]

mod common;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use common::{Options, start};
use serde_json::Value;

fn git(dir: &std::path::Path, args: &[&str]) -> anyhow::Result<()> {
    let o = std::process::Command::new("git").arg("-C").arg(dir).args(args).env("GIT_AUTHOR_NAME", "T").env("GIT_AUTHOR_EMAIL", "t@example.com").env("GIT_COMMITTER_NAME", "T").env("GIT_COMMITTER_EMAIL", "t@example.com").output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(())
}

/// The pool now: (starting, idle, busy), and its executors' ids.
fn pool(port: u16, admin: &str) -> anyhow::Result<((u64, u64, u64), Vec<String>, Value)> {
    let (_, body) = common::http(port, "GET", "/api/ci/pools/burst", admin, None)?;
    let v: Value = serde_json::from_str(&body).unwrap_or_default();
    let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let ids = v.get("executors").and_then(Value::as_array).map(|a| a.iter().filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_owned)).collect()).unwrap_or_default();
    Ok(((n("starting"), n("idle"), n("busy")), ids, v))
}

/// Removes the containers of the executors the test saw (the server's runner is killed with the
/// test, before it can).
fn remove(seen: &BTreeSet<String>) {
    for id in seen {
        if let Ok(o) = std::process::Command::new("docker").args(["ps", "-aq", "--filter", &format!("label=kiln.leg={id}")]).output() {
            for c in String::from_utf8_lossy(&o.stdout).split_whitespace() {
                let _ = std::process::Command::new("docker").args(["rm", "-f", c]).output();
            }
        }
        if let Ok(o) = std::process::Command::new("docker").args(["network", "ls", "-q", "--filter", &format!("label=kiln.leg={id}")]).output() {
            for n in String::from_utf8_lossy(&o.stdout).split_whitespace() {
                let _ = std::process::Command::new("docker").args(["network", "rm", n]).output();
            }
        }
    }
}

/// W13-AC04: a burst of 200 legs on a `docker` pool with `min_idle = 2` and `max = 32`: its
/// executors scale to 32, the queue drains, and they're back to 2 within `idle_timeout`.
#[test]
fn w13_ac04_docker_pool_burst() -> anyhow::Result<()> {
    if !std::process::Command::new("docker").args(["info", "--format", "{{.ServerVersion}}"]).output().is_ok_and(|o| o.status.success()) {
        eprintln!("w13_ac04_docker_pool_burst: no Docker here");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::write(repo.join("README.md"), "burst\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!(
        "[[ci.pools]]\nname = \"burst\"\nexecutor = \"docker\"\nhosts = \"executors contains docker\"\nimage = \"kiln/runner-ubuntu-24.04\"\nlabels = [\"burst\"]\nmin_idle = 2\nmax = 32\nper_host_max = 32\nidle_timeout = \"20s\"\n\n[[repos]]\nname = \"acme/burst\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n",
        repo.display()
    );
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: &extra })?;
    let (port, admin) = (s.port, s.admin.clone());
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let result = (|| -> anyhow::Result<()> {
        // min_idle ready before anything asks.
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let (c, ids, v) = pool(port, &admin)?;
            seen.extend(ids);
            if c == (0, 2, 0) {
                break;
            }
            anyhow::ensure!(Instant::now() < deadline, "min_idle never ready: {v}\n{}", s.log());
            std::thread::sleep(Duration::from_millis(250));
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let (_, body) = common::http(port, "GET", "/api/repos", &admin, None)?;
            if body.contains("acme/burst") && !body.contains("\"last_polled_at\":null") {
                break;
            }
            anyhow::ensure!(Instant::now() < deadline, "never polled: {body}");
            std::thread::sleep(Duration::from_millis(300));
        }
        let cells: Vec<String> = (0..200).map(|i| i.to_string()).collect();
        std::fs::write(
            repo.join(".kiln/ci.yaml"),
            format!("name: burst\non: {{ push: {{}} }}\njobs:\n  leg:\n    runs-on: burst\n    strategy:\n      matrix:\n        i: [{}]\n    steps:\n      - run: echo \"cell ${{{{ matrix.i }}}} in $(hostname)\"\n", cells.join(", ")),
        )?;
        git(&repo, &["add", "."])?;
        git(&repo, &["commit", "-qm", "burst"])?;
        // The burst: executors to max, never past it, the queue drained.
        let (mut peak, deadline) = (0u64, Instant::now() + Duration::from_secs(1200));
        let run = loop {
            let (c, ids, v) = pool(port, &admin)?;
            seen.extend(ids);
            let total = c.0 + c.1 + c.2;
            anyhow::ensure!(total <= 32, "past max: {v}");
            peak = peak.max(total);
            let (_, body) = common::http(port, "GET", "/api/ci/runs?repo=acme/burst&limit=5", &admin, None)?;
            let runs: Value = serde_json::from_str(&body).unwrap_or_default();
            if let Some(r) = runs.get("runs").and_then(Value::as_array).and_then(|r| r.first()).filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())).cloned() {
                break r;
            }
            anyhow::ensure!(Instant::now() < deadline, "the burst never finished: {v}\n{body}");
            std::thread::sleep(Duration::from_millis(250));
        };
        let drained = Instant::now();
        let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
        let (_, detail) = common::http(port, "GET", &format!("/api/ci/runs/{id}"), &admin, None)?;
        let d: Value = serde_json::from_str(&detail)?;
        let legs = d.get("legs").and_then(Value::as_array).cloned().unwrap_or_default();
        anyhow::ensure!(legs.len() == 200, "200 legs, not {}", legs.len());
        let failed: Vec<&Value> = legs.iter().filter(|l| l.get("conclusion").and_then(Value::as_str) != Some("success")).collect();
        anyhow::ensure!(failed.is_empty(), "every leg succeeds: {:?}", failed.first());
        anyhow::ensure!(run.get("conclusion").and_then(Value::as_str) == Some("success"), "{run}");
        anyhow::ensure!(peak == 32, "the executors scale to max: peak {peak}");
        // Each leg ran in a pool executor of its own.
        let first = legs.first().and_then(|l| l.get("id")).and_then(Value::as_str).unwrap_or_default();
        let mut log = String::new();
        for step in 0..6 {
            let (_, text) = common::http(port, "GET", &format!("/api/ci/legs/{first}/logs?step={step}"), &admin, None)?;
            log.push_str(&text);
        }
        anyhow::ensure!(log.contains("Running in pool burst's executor"), "{log}");
        // Back to min_idle within idle_timeout (20 s), with a little slack for the loaded machine.
        let deadline = drained + Duration::from_secs(20 + 25);
        loop {
            let (c, ids, v) = pool(port, &admin)?;
            seen.extend(ids);
            if c == (0, 2, 0) {
                break;
            }
            anyhow::ensure!(Instant::now() < deadline, "not back to min_idle within idle_timeout: {v}");
            std::thread::sleep(Duration::from_millis(500));
        }
        Ok(())
    })();
    remove(&seen);
    result
}
