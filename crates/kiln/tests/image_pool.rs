//! W13-AC07 (docs/ci.md), its Dockerfile half: an image built in kiln from what its editor holds
//! (the Images panel's Build and push, `POST /api/ci/images/build`), pushed to kiln's registry and
//! listed with its platform; then a `docker` pool of that image (made as the Pools page makes one),
//! pulled from kiln's registry with a token of the pool's, runs a leg in it. (The Agentfile half waits for shards, W13-R06.) Needs
//! Docker with buildx, and pulls `ubuntu:24.04`.

#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use common::{Options, start};
use serde_json::{Value, json};

fn git(dir: &std::path::Path, args: &[&str]) -> anyhow::Result<()> {
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(())
}

/// What a test leaves in the engine: the image's tags, and the pool's containers.
struct Leftovers {
    images: Vec<String>,
}

impl Drop for Leftovers {
    fn drop(&mut self) {
        for i in &self.images {
            let _ = std::process::Command::new("docker").args(["image", "rm", "-f", i]).output();
        }
        if let Ok(o) = std::process::Command::new("docker").args(["ps", "-aq", "--filter", "label=kiln.pool=built"]).output() {
            for c in String::from_utf8_lossy(&o.stdout).split_whitespace() {
                let _ = std::process::Command::new("docker").args(["rm", "-f", c]).output();
            }
        }
    }
}

/// A run's conclusion and its first leg's log and record, once it's done.
fn finished(port: u16, admin: &str, run: &str, log: &dyn Fn() -> String) -> anyhow::Result<(String, String, Value)> {
    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        let (_, body) = common::http(port, "GET", &format!("/api/ci/runs/{run}"), admin, None)?;
        let d: Value = serde_json::from_str(&body).unwrap_or_default();
        if d.pointer("/run/finished_at").is_some_and(|f| !f.is_null()) {
            let leg = d.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let (_, about) = common::http(port, "GET", &format!("/api/ci/legs/{leg}"), admin, None)?;
            let about: Value = serde_json::from_str(&about).unwrap_or_default();
            let mut text = String::new();
            for step in about.get("steps").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.get("idx").and_then(Value::as_u64)) {
                text.push_str(&common::http(port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), admin, None)?.1);
            }
            return Ok((d.pointer("/run/conclusion").and_then(Value::as_str).unwrap_or_default().to_owned(), text, about));
        }
        anyhow::ensure!(Instant::now() < deadline, "{run} didn't finish: {body}\n{}", log());
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[test]
fn w13_ac07_a_pool_runs_a_leg_in_an_image_built_here() -> anyhow::Result<()> {
    if !std::process::Command::new("docker").args(["buildx", "version"]).output().is_ok_and(|o| o.status.success()) {
        eprintln!("w13_ac07: no docker buildx here");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::write(repo.join("README.md"), "images\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!("[[repos]]\nname = \"acme/images\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: &extra })?;
    let (port, admin) = (s.port, s.admin.clone());
    let image = format!("127.0.0.1:{port}/acme/images/images/pooled");
    let _left = Leftovers { images: vec![format!("{image}:latest")] };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = common::http(port, "GET", "/api/repos", &admin, None)?;
        if body.contains("acme/images") && !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }

    // The image, as its editor has it: built on the server's runner, pushed, and listed.
    let dockerfile = "FROM ubuntu:24.04\nRUN echo built-by-kiln > /built-by-kiln\n";
    let (st, body) = common::http(port, "POST", "/api/ci/images/build?repo=acme/images", &admin, Some(&json!({ "name": "pooled", "content": dockerfile, "runs_on": "local", "push": true })))?;
    anyhow::ensure!(st == 201, "{st} {body}");
    let built: Value = serde_json::from_str(&body)?;
    let (conclusion, log, _) = finished(port, &admin, built.get("run").and_then(Value::as_str).unwrap_or_default(), &|| s.log())?;
    assert_eq!(conclusion, "success", "{log}\n{}", s.log());
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        a => a,
    };
    let (_, body) = common::http(port, "GET", "/api/ci/images?repo=acme/images", &admin, None)?;
    let images: Value = serde_json::from_str(&body)?;
    assert_eq!(images.pointer("/images/0/name"), Some(&json!("pooled")), "{body}");
    assert_eq!(images.pointer("/images/0/platforms"), Some(&json!([format!("linux/{arch}")])), "{body}");

    // A pool of it, as the Pools page makes one, and a leg routed to the pool.
    let spec = json!({ "executor": "docker", "hosts": "executors contains docker", "image": format!("{image}:latest"), "labels": ["built"], "min_idle": 1, "max": 2, "idle_timeout": "30s" });
    let (st, body) = common::http(port, "PUT", "/api/ci/pools/built", &admin, Some(&spec))?;
    anyhow::ensure!(st == 200, "the pool: {st} {body}");
    std::fs::write(
        repo.join(".kiln/pooled.yaml"),
        "name: pooled\non: { push: {} }\njobs:\n  inside:\n    runs-on: built\n    checkout: false\n    steps:\n      - run: echo \"$(cat /built-by-kiln) in $(. /etc/os-release && echo $ID)\"\n",
    )?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "a leg in the pool"])?;
    let deadline = Instant::now() + Duration::from_secs(300);
    let run = loop {
        let (_, body) = common::http(port, "GET", "/api/ci/runs?repo=acme/images&limit=10", &admin, None)?;
        let runs: Value = serde_json::from_str(&body).unwrap_or_default();
        if let Some(r) = runs.get("runs").and_then(Value::as_array).into_iter().flatten().find(|r| r.get("workflow").and_then(Value::as_str).is_some_and(|w| w.ends_with("pooled.yaml"))) {
            break r.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        }
        anyhow::ensure!(Instant::now() < deadline, "the pool's run never started: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let (conclusion, log, about) = finished(port, &admin, &run, &|| s.log())?;
    assert_eq!(conclusion, "success", "{log}\n{about}\n{}", s.log());
    assert!(log.contains("built-by-kiln in ubuntu"), "the leg ran in the image built here: {log}");
    assert_eq!(about.pointer("/leg/executor"), Some(&json!("docker")), "{about}");
    let (_, body) = common::http(port, "GET", "/api/ci/pools/built", &admin, None)?;
    let p: Value = serde_json::from_str(&body)?;
    assert_eq!(p.get("image").or_else(|| p.pointer("/pool/image")), Some(&json!(format!("{image}:latest"))), "{body}");
    Ok(())
}
