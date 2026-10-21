//! W15-R02 (docs/ci.md): a `container` environment's deployment. `kiln/deploy@v1` with a Compose
//! file goes up on the runner the environment's routing chose, as the environment's project, and
//! the step waits for its health check; the containers outlive the job. A job deploys only to the
//! environment it's bound to. Needs Docker with Compose, and pulls `alpine:3.20`.

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

/// Docker with Compose here.
fn compose() -> bool {
    Command::new("docker").args(["compose", "version"]).output().is_ok_and(|o| o.status.success())
}

/// What a test deployed, taken down when it ends: its containers, then its network (left behind,
/// each holds one of the engine's few address pools).
struct Project(String);

impl Drop for Project {
    fn drop(&mut self) {
        let label = format!("label=com.docker.compose.project={}", self.0);
        let ids = |list: &[&str]| Command::new("docker").args(list).args(["--filter", &label]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        for id in ids(&["ps", "-aq"]).split_whitespace() {
            let _ = Command::new("docker").args(["rm", "-f", id]).output();
        }
        for id in ids(&["network", "ls", "-q"]).split_whitespace() {
            let _ = Command::new("docker").args(["network", "rm", id]).output();
        }
    }
}

const COMPOSE: &str = "services:\n  web:\n    image: alpine:3.20\n    command: [\"sh\", \"-c\", \"sleep 2 && touch /tmp/up && exec sleep 3600\"]\n    healthcheck:\n      test: [\"CMD\", \"test\", \"-f\", \"/tmp/up\"]\n      interval: 1s\n      retries: 60\n";

/// W15-R02: the deployment up and healthy, outliving its job; another environment refused.
#[test]
fn w15_r02_a_container_environment_deploys_with_compose() -> anyhow::Result<()> {
    if !compose() {
        eprintln!("skipped: no Docker with Compose here");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    // An environment of this run's own, so its project is too.
    let env = format!("st{}", std::process::id());
    let project = Project(format!("ship-{env}"));
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/ship", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (st, body) = http(s.port, "PUT", &format!("/api/ci/environments/{env}?repo=acme/ship"), &s.admin, Some(&json!({ "kind": "container", "targets": { "runners": "label local" } })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/ship.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "ship\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::write(work.join("compose.yaml"), COMPOSE)?;
    let workflow = format!(
        "on: push\njobs:\n  deploy:\n    runs-on: local\n    environment: {env}\n    steps:\n      - uses: kiln/deploy@v1\n        with:\n          environment: {env}\n          compose: compose.yaml\n          timeout: 5m\n  elsewhere:\n    runs-on: local\n    environment: {env}\n    steps:\n      - uses: kiln/deploy@v1\n        with:\n          environment: production\n          compose: compose.yaml\n"
    );
    std::fs::write(work.join(".kiln/ship.yaml"), workflow)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "deploy"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let mut run = String::new();
    until(&s, 600, "the run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/ship&limit=5", &s.admin, None)?;
        let r = serde_json::from_str::<Value>(&body)?.pointer("/runs/0").cloned().unwrap_or_default();
        run = r.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok((run.is_empty() || r.get("finished_at").is_none_or(Value::is_null)).then_some(body))
    })?;
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
    let mut legs = std::collections::BTreeMap::new();
    for l in serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
        let id = l.get("id").and_then(Value::as_str).unwrap_or_default();
        let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{id}"), &s.admin, None)?;
        let about: Value = serde_json::from_str(&about)?;
        let (_, log) = http(s.port, "GET", &format!("/api/ci/legs/{id}/logs?step=0"), &s.admin, None)?;
        let key = about.pointer("/leg/key").and_then(Value::as_str).unwrap_or_default().to_owned();
        legs.insert(key, (about, log));
    }
    let (about, log) = legs.get("deploy").cloned().unwrap_or_default();
    assert_eq!(about.pointer("/leg/conclusion"), Some(&json!("success")), "{about}\n{log}\n{}", s.log());
    assert!(log.contains(&format!("Deploying {} to {env}", project.0)) && log.contains("web (alpine:3.20): running, healthy"), "{log}");
    // Still running after its job, as a deployment is.
    let ps = Command::new("docker").args(["ps", "--filter", &format!("label=com.docker.compose.project={}", project.0), "--format", "{{.Status}}"]).output()?;
    let status = String::from_utf8_lossy(&ps.stdout).into_owned();
    assert!(status.contains("Up") && status.contains("healthy"), "the deployment runs on: {status}");
    // Not to an environment the job isn't bound to.
    let (about, log) = legs.get("elsewhere").cloned().unwrap_or_default();
    assert_eq!(about.pointer("/leg/conclusion"), Some(&json!("failure")), "{about}\n{log}");
    assert!(about.to_string().contains("deploys only to the environment it's bound to") || log.contains("deploys only to the environment it's bound to"), "{about}\n{log}");
    Ok(())
}
