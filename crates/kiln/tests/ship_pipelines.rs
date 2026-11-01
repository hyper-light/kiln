//! Ship's Pipelines (docs/ci.md W29-R02, R03), against a server of its own with its own runner and
//! real `git`. A repository kiln hosts, with a workflow of each dialect, a reusable workflow and an
//! action of each kind, gives one row for each, standing as `kiln_ci::pipelines` says. A run that
//! fails shows on the next read, and a `pipelines` event names the repository. A caller who may
//! not read the repository doesn't see it.

#![cfg(unix)]

mod common;

use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{Options, Server, http, start};
use serde_json::{Value, json};

/// `git` with no configuration but this test's, in `dir`.
fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git")
        .args(["-c", "user.name=Ada", "-c", "user.email=ada@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    anyhow::ensure!(o.status.success(), "git {args:?}: {out}");
    Ok(out)
}

/// A user with `grants`, and a personal access token that may do what they may.
fn person(s: &Server, name: &str, grants: &[&str]) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": grants })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}'s token: {st} {body}");
    serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))
}

fn pipelines(s: &Server, token: &str, query: &str) -> anyhow::Result<Value> {
    let (st, body) = http(s.port, "GET", &format!("/api/ship/pipelines{query}"), token, None)?;
    anyhow::ensure!(st == 200, "GET /api/ship/pipelines{query}: {st} {body}");
    Ok(serde_json::from_str(&body)?)
}

/// The rows of `repo` in a list.
fn rows_of(v: &Value, repo: &str) -> Vec<Value> {
    v.get("repos")
        .and_then(Value::as_array)
        .and_then(|repos| repos.iter().find(|r| r.get("repo").and_then(Value::as_str) == Some(repo)))
        .and_then(|r| r.get("rows").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

fn row<'a>(rows: &'a [Value], path: &str) -> Option<&'a Value> {
    rows.iter().find(|r| r.get("path").and_then(Value::as_str) == Some(path))
}

fn field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// The events a server sends (`/api/events`), gathered until `until` passes or `stop` says.
fn events(port: u16, token: &str, until: Instant, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<String> {
    let token = token.to_owned();
    std::thread::spawn(move || {
        let mut got = Vec::new();
        let Ok(mut sock) = std::net::TcpStream::connect(("127.0.0.1", port)) else { return String::new() };
        let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
        // A WebSocket: the server's frames are unmasked, so what it sends is there as it is.
        let _ = write!(sock, "GET /api/events HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n");
        let mut buf = [0u8; 16 << 10];
        while Instant::now() < until && !stop.load(Ordering::SeqCst) {
            match sock.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(buf.get(..n).unwrap_or_default()),
                Err(_) => {}
            }
        }
        String::from_utf8_lossy(&got).into_owned()
    })
}

const FILES: &[(&str, &str)] = &[
    (".github/workflows/ci.yml", "name: GitHub CI\non: [push, pull_request]\njobs:\n  build:\n    runs-on: local\n    steps:\n      - run: echo built\n"),
    (
        ".github/workflows/rust.yml",
        "name: rust-check\non:\n  workflow_call:\n    inputs:\n      toolchain:\n        type: string\n        default: stable\njobs:\n  check:\n    runs-on: local\n    steps:\n      - run: echo checked\n",
    ),
    // Long enough that the first read finds it under way, and then it fails.
    (".kiln/ci.yaml", "name: ci\non:\n  push:\n    branches: [main]\njobs:\n  test:\n    runs-on: local\n    steps:\n      - run: sleep 6 && exit 1\n"),
    (".github/actions/setup/action.yml", "name: setup\ndescription: Sets things up\nruns:\n  using: composite\n  steps:\n    - run: echo set up\n      shell: bash\n"),
    (".github/actions/comment/action.yml", "name: pr-comment\nruns:\n  using: node24\n  main: index.js\n"),
    (".github/actions/comment/index.js", "console.log('commented');\n"),
    (".github/actions/image/action.yml", "name: build-image\nruns:\n  using: docker\n  image: Dockerfile\n"),
    (".github/actions/image/Dockerfile", "FROM alpine:3.20\n"),
    (".kiln/actions/fuzz/action.yml", "name: fuzz\nruns:\n  using: firecracker\n  image: docker://alpine:3.20\n"),
    (".kiln/actions/sandbox/action.yml", "name: sandbox\nruns:\n  using: shards\n  image: docker://alpine:3.20\n"),
];

#[test]
fn w29_r03_pipelines_read_model() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // The engine runs `.kiln/` workflows; GitHub workflows run only where a commit has none.
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true\ngithub_workflows = \"auto\"", extra: "" })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;

    // A repository kiln hosts, its first trunk pushed by its owner.
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    git(&home, dir.path(), &["clone", "-q", &format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port), "app"])?;
    let work = dir.path().join("app");
    // Its first poll takes the branch as it is: the workflows come after, as a push that runs them.
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;
    let polled = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let list = v.as_array().cloned().or_else(|| v.get("repos").and_then(Value::as_array).cloned()).unwrap_or_default();
        if list.iter().any(|r| field(r, "name") == "acme/app" && r.get("last_polled_at").is_some_and(|t| !t.is_null())) {
            break;
        }
        anyhow::ensure!(Instant::now() < polled, "acme/app's first poll: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    for (path, text) in FILES {
        let p = work.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, text)?;
    }
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "pipelines"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;
    let head = git(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();

    // One row for each workflow and action, each with what Ship's rows carry.
    let deadline = Instant::now() + Duration::from_secs(60);
    let rows = loop {
        let rows = rows_of(&pipelines(&s, &s.admin, "?repo=acme/app")?, "acme/app");
        if rows.len() == 8 {
            break rows;
        }
        anyhow::ensure!(Instant::now() < deadline, "8 rows for acme/app at {head}: {rows:?}");
        std::thread::sleep(Duration::from_millis(500));
    };
    for (path, kind) in [
        (".github/workflows/ci.yml", "workflow"),
        (".github/workflows/rust.yml", "reusable"),
        (".kiln/ci.yaml", "workflow"),
        (".github/actions/setup/action.yml", "composite"),
        (".github/actions/comment/action.yml", "javascript"),
        (".github/actions/image/action.yml", "docker"),
        (".kiln/actions/fuzz/action.yml", "firecracker"),
        (".kiln/actions/sandbox/action.yml", "shards"),
    ] {
        let r = row(&rows, path).ok_or_else(|| anyhow::anyhow!("no row for {path}: {rows:?}"))?;
        assert_eq!(field(r, "kind"), kind, "{r}");
        for key in ["name", "path", "stand", "word", "line", "ref", "commit"] {
            assert!(!field(r, key).is_empty(), "{path}'s {key}: {r}");
        }
        assert_eq!((field(r, "ref"), field(r, "commit")), ("main", head.as_str()), "{r}");
        assert_eq!(r.get("pips").and_then(Value::as_array).map(Vec::len), Some(3), "{r}");
        assert!(r.get("draft").is_some_and(Value::is_null) && r.get("who").is_some() && r.get("when").is_some(), "{r}");
        assert_eq!(r.get("dialect").is_some_and(|d| !d.is_null()), matches!(kind, "workflow" | "reusable"), "a workflow has a dialect, an action none: {r}");
    }
    let sandbox = row(&rows, ".kiln/actions/sandbox/action.yml").cloned().unwrap_or_default();
    assert_eq!(field(&sandbox, "word"), "Can't run", "{sandbox}");
    assert_eq!(sandbox.pointer("/pips/1").and_then(Value::as_str), Some("blocked"), "{sandbox}");
    let github = row(&rows, ".github/workflows/ci.yml").cloned().unwrap_or_default();
    assert_eq!((field(&github, "word"), field(&github, "stand")), ("Not run here", "superseded"), "{github}");
    assert_eq!(field(&github, "dialect"), "gha");
    assert_eq!(field(row(&rows, ".kiln/ci.yaml").unwrap_or(&Value::Null), "dialect"), "kiln");

    // The kiln workflow's run fails: the next read says so, and a `pipelines` event names the
    // repository.
    let stop = Arc::new(AtomicBool::new(false));
    let heard = events(s.port, &s.admin, Instant::now() + Duration::from_secs(120), Arc::clone(&stop));
    let deadline = Instant::now() + Duration::from_secs(180);
    let run = loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=50", &s.admin, None)?;
        let runs: Vec<Value> = serde_json::from_str::<Value>(&body).unwrap_or_default().get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        if let Some(r) = runs.iter().find(|r| field(r, "workflow") == ".kiln/ci.yaml" && r.get("finished_at").is_some_and(|f| !f.is_null())) {
            break r.clone();
        }
        anyhow::ensure!(Instant::now() < deadline, "no finished run of .kiln/ci.yaml at {head}: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    assert!(matches!(field(&run, "conclusion"), "failure" | "failed") || field(&run, "status") == "failed", "{run}");
    let deadline = Instant::now() + Duration::from_secs(30);
    let ci = loop {
        let rows = rows_of(&pipelines(&s, &s.admin, "?repo=acme/app")?, "acme/app");
        let ci = row(&rows, ".kiln/ci.yaml").cloned().unwrap_or_default();
        if field(&ci, "word") == "Failing" || Instant::now() >= deadline {
            break ci;
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    assert_eq!((field(&ci, "word"), field(&ci, "stand")), ("Failing", "failing"), "{ci}");
    assert_eq!(ci.pointer("/pips/2").and_then(Value::as_str), Some("failed"), "{ci}");
    std::thread::sleep(Duration::from_secs(2));
    stop.store(true, Ordering::SeqCst);
    let frames = heard.join().unwrap_or_default();
    assert!(frames.contains(r#""type":"pipelines","repo":"acme/app""#), "no pipelines event for acme/app in {} bytes of events", frames.len());

    // A caller who may read another repository only doesn't see this one.
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/other", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "acme/other: {st} {body}");
    let other = person(&s, "other", &["viewer@repo:acme/other"])?;
    let theirs = pipelines(&s, &other, "")?;
    assert!(rows_of(&theirs, "acme/app").is_empty(), "{theirs}");
    assert!(!theirs.to_string().contains("acme/app"), "{theirs}");
    Ok(())
}
