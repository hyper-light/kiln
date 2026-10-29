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

/// One look of a loop that waits until its own deadline: a request that failed (common::http's
/// 30 s read timeout, the server slow mid-burst on a loaded machine) is "not yet" until then, not
/// the test's end.
fn polled<T>(look: anyhow::Result<T>, deadline: Instant, what: &str) -> anyhow::Result<Option<T>> {
    match look {
        Ok(v) => Ok(Some(v)),
        Err(e) if Instant::now() < deadline => {
            eprintln!("{what}: {e:#}; again");
            Ok(None)
        }
        Err(e) => Err(e),
    }
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
            let Some((c, ids, v)) = polled(pool(port, &admin), deadline, "the pool")? else {
                std::thread::sleep(Duration::from_millis(500));
                continue;
            };
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
            let Some((c, ids, v)) = polled(pool(port, &admin), deadline, "the pool")? else {
                std::thread::sleep(Duration::from_millis(250));
                continue;
            };
            seen.extend(ids);
            let total = c.0 + c.1 + c.2;
            anyhow::ensure!(total <= 32, "past max: {v}");
            peak = peak.max(total);
            let Some((_, body)) = polled(common::http(port, "GET", "/api/ci/runs?repo=acme/burst&limit=5", &admin, None), deadline, "the runs")? else {
                std::thread::sleep(Duration::from_millis(250));
                continue;
            };
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
            let Some((c, ids, v)) = polled(pool(port, &admin), deadline, "the pool")? else {
                std::thread::sleep(Duration::from_millis(500));
                continue;
            };
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

/// W21-R08: a pool's scaling history. Its size and the legs waiting for it are taken from the
/// server's start, a minute apart: here a pool no runner can start, nothing in it, nothing waiting.
#[test]
fn w21_r08_a_pools_history() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let extra = "[[ci.pools]]\nname = \"quiet\"\nexecutor = \"docker\"\nimage = \"kiln/runner-ubuntu-24.04\"\nlabels = [\"quiet\"]\nmin_idle = 0\nmax = 2\n";
    let s = start(dir.path(), &Options { extra, ..Options::default() })?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let history = loop {
        let (st, body) = common::http(s.port, "GET", "/api/ci/pools/quiet/history", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        if st == 200 && v.get("samples").and_then(Value::as_array).is_some_and(|l| !l.is_empty()) {
            break v;
        }
        anyhow::ensure!(Instant::now() < deadline, "no sample: {st} {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(300));
    };
    let first = history.pointer("/samples/0").cloned().unwrap_or_default();
    for k in ["starting", "idle", "busy", "waiting"] {
        assert_eq!(first.get(k).and_then(Value::as_i64), Some(0), "{k}: {history}");
    }
    assert!(first.get("ts").and_then(Value::as_i64).is_some_and(|t| t > 0), "{history}");
    assert_eq!(history.get("hours").and_then(Value::as_i64), Some(24), "a day by default: {history}");
    // A day by default, an hour at least, a week at most.
    for (asked, got) in [("0", 1), ("9999", 168)] {
        let (_, body) = common::http(s.port, "GET", &format!("/api/ci/pools/quiet/history?hours={asked}"), &s.admin, None)?;
        assert_eq!(serde_json::from_str::<Value>(&body)?.get("hours").and_then(Value::as_i64), Some(got), "{asked}: {body}");
    }
    // Who may not read the fleet doesn't read it.
    let (st, body) = common::http(s.port, "POST", "/api/users", &s.admin, Some(&serde_json::json!({ "name": "nobody", "grants": [] })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let (_, body) = common::http(s.port, "POST", "/api/tokens", &s.admin, Some(&serde_json::json!({ "kind": "pat", "user": "nobody", "grants": ["owner"] })))?;
    let nobody = serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (st, _) = common::http(s.port, "GET", "/api/ci/pools/quiet/history", &nobody, None)?;
    assert_eq!(st, 403);
    Ok(())
}
