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

/// What code mode's `program` returns, run by the admin over MCP.
fn code(s: &Server, program: &str) -> anyhow::Result<Value> {
    let (st, body) = http(s.port, "POST", "/api/mcp", &s.admin, Some(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "code_run", "arguments": { "program": program } } })))?;
    anyhow::ensure!(st == 200, "code_run: {st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    let text = v.pointer("/result/content/0/text").and_then(Value::as_str).unwrap_or_default();
    anyhow::ensure!(v.pointer("/result/isError") == Some(&json!(false)), "{program}: {text}");
    Ok(serde_json::from_str::<Value>(text)?.get("result").cloned().unwrap_or_default())
}

/// W29-R09: code mode's `program` answers as `GET route` does for the admin. The route is read
/// before and after it, until the two agree: a run may finish between them.
fn as_route(s: &Server, program: &str, route: &str) -> anyhow::Result<()> {
    let read = || -> anyhow::Result<Value> {
        let (st, body) = http(s.port, "GET", route, &s.admin, None)?;
        anyhow::ensure!(st == 200, "GET {route}: {st} {body}");
        Ok(serde_json::from_str(&body)?)
    };
    for _ in 0..10 {
        let before = read()?;
        let got = code(s, program)?;
        if read()? == before {
            assert_eq!(got, before, "{program} answers as GET {route}");
            return Ok(());
        }
    }
    anyhow::bail!("GET {route} changed each time it was read")
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

    // Agents have the list as an operation (W29-R09).
    as_route(&s, "return await kiln.ship.pipelines.list({})", "/api/ship/pipelines")?;
    as_route(&s, "return await kiln.ship.pipelines.list({ repo: 'acme/app', q: 'fail' })", "/api/ship/pipelines?repo=acme/app&q=fail")?;

    // A caller who may read another repository only doesn't see this one.
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/other", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "acme/other: {st} {body}");
    let other = person(&s, "other", &["viewer@repo:acme/other"])?;
    let theirs = pipelines(&s, &other, "")?;
    assert!(rows_of(&theirs, "acme/app").is_empty(), "{theirs}");
    assert!(!theirs.to_string().contains("acme/app"), "{theirs}");
    Ok(())
}

/// Every one of GitHub's events, as `on:` names it, in a reusable GitHub-dialect workflow.
const EVERY_GITHUB_EVENT: &str = "name: every event\non:\n  branch_protection_rule:\n  check_run:\n  check_suite:\n  create:\n  delete:\n  deployment:\n  deployment_status:\n  discussion:\n  discussion_comment:\n  fork:\n  gollum:\n  image_version:\n  issue_comment:\n  issues:\n  label:\n  merge_group:\n  milestone:\n  page_build:\n  project:\n  project_card:\n  project_column:\n  public:\n  pull_request:\n    branches: [main]\n  pull_request_review:\n  pull_request_review_comment:\n  pull_request_target:\n  push:\n    branches: [main]\n  registry_package:\n  release:\n    types: [published]\n  repository_dispatch:\n  schedule:\n    - cron: '0 3 * * *'\n  status:\n  watch:\n  workflow_call:\n    inputs:\n      toolchain:\n        type: string\n        required: true\n    outputs:\n      built:\n        value: ${{ jobs.build.outputs.built }}\n    secrets:\n      TOKEN:\n        required: true\n  workflow_dispatch:\n  workflow_run:\n    workflows: [ci]\n    types: [completed]\njobs:\n  build:\n    runs-on: local\n    outputs:\n      built: yes\n    steps:\n      - uses: actions/checkout@v4\n      - run: echo ${{ secrets.TOKEN }} ${{ vars.REGION }}\n";

/// Every one of kiln's events, in a kiln-dialect workflow.
const EVERY_KILN_EVENT: &str = "name: every kiln event\non:\n  diff:\n  land:\n  review:\n  comment:\n  pipeline:\n    workflows: [ci]\n  agent:\n  deploy:\n  webhook:\n  manual:\n  tag:\njobs:\n  check:\n    runs-on: local\n    steps:\n      - run: echo checked\n";

const ACTIONS: &[(&str, &str)] = &[
    (".github/actions/setup/action.yml", "name: setup\ninputs:\n  toolchain:\n    required: true\noutputs:\n  path:\n    description: Where it went\nruns:\n  using: composite\n  steps:\n    - name: Install\n      run: echo installing\n      shell: bash\n"),
    (".github/actions/comment/action.yml", "name: pr-comment\nruns:\n  using: node24\n  main: index.js\n"),
    (".github/actions/image/action.yml", "name: build-image\nruns:\n  using: docker\n  image: Dockerfile\n  entrypoint: /entry.sh\n"),
    (".kiln/actions/fuzz/action.yml", "name: fuzz\nruns:\n  using: firecracker\n  image: docker://alpine:3.20\n  resources:\n    cpus: 2\n  egress: [crates.io]\n"),
    (".kiln/actions/sandbox/action.yml", "name: sandbox\nruns:\n  using: shards\n  image: docker://alpine:3.20\n"),
];

fn pane(s: &Server, token: &str, path: &str, at: Option<&str>) -> anyhow::Result<(u16, Value)> {
    let query = match at {
        Some(r) => format!("?path={path}&ref={r}"),
        None => format!("?path={path}"),
    };
    let (st, body) = http(s.port, "GET", &format!("/api/ship/pipelines/acme/kit{query}"), token, None)?;
    Ok((st, serde_json::from_str(&body).unwrap_or(Value::Null)))
}

fn group<'a>(p: &'a Value, name: &str) -> Vec<&'a Value> {
    p.get("groups")
        .and_then(Value::as_array)
        .and_then(|g| g.iter().find(|g| field(g, "name") == name))
        .and_then(|g| g.get("lines").and_then(Value::as_array))
        .map(|l| l.iter().collect())
        .unwrap_or_default()
}

/// The text at a JSON pointer, or nothing.
fn at<'a>(v: &'a Value, pointer: &str) -> &'a str {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or_default()
}

fn group_names(p: &Value) -> Vec<String> {
    p.get("groups").and_then(Value::as_array).map(|g| g.iter().map(|g| field(g, "name").to_owned()).collect()).unwrap_or_default()
}

#[test]
fn w29_r04_the_pane() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: false, ci: "kiln_workflows = true\ngithub_workflows = \"auto\"", extra: "" })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/kit", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    git(&home, dir.path(), &["clone", "-q", &format!("http://x:{}@127.0.0.1:{}/git/acme/kit.git", s.admin, s.port), "kit"])?;
    let work = dir.path().join("kit");
    let mut files: Vec<(&str, &str)> = vec![(".github/workflows/every.yml", EVERY_GITHUB_EVENT), (".kiln/every.yaml", EVERY_KILN_EVENT), (".github/actions/comment/index.js", "console.log('hi');\n"), (".github/actions/image/Dockerfile", "FROM alpine:3.20\n")];
    files.extend_from_slice(ACTIONS);
    for (path, text) in &files {
        let p = work.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, text)?;
    }
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "the kit"])?;
    let tagged = git(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();
    git(&home, &work, &["tag", "v1.0.0"])?;
    // The workflow changes after its release.
    std::fs::write(work.join(".github/workflows/every.yml"), EVERY_GITHUB_EVENT.replacen("name: every event", "name: every event, renamed", 1))?;
    git(&home, &work, &["commit", "-qam", "renamed"])?;
    git(&home, &work, &["push", "-q", "origin", "main", "v1.0.0"])?;
    let head = git(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();

    // The two workflows' Triggers: 46 lines between them, each with its group and words.
    let (st, gha) = pane(&s, &s.admin, ".github/workflows/every.yml", None)?;
    anyhow::ensure!(st == 200, "the GitHub workflow's pane: {st} {gha}");
    let (st, kiln) = pane(&s, &s.admin, ".kiln/every.yaml", None)?;
    anyhow::ensure!(st == 200, "the kiln workflow's pane: {st} {kiln}");
    let lines: Vec<&Value> = group(&gha, "Triggers").into_iter().chain(group(&kiln, "Triggers")).collect();
    assert_eq!(lines.len(), 46, "{lines:?}");
    assert!(lines.iter().all(|l| !field(l, "name").is_empty() && !field(l, "group").is_empty()), "{lines:?}");
    assert_eq!((at(&gha, "/head/title"), at(&gha, "/head/commit"), at(&gha, "/head/kind")), ("every event, renamed", head.as_str(), "reusable"), "{gha}");
    assert!(gha.pointer("/head/draft").is_some_and(Value::is_null));
    // A reusable workflow's interface: its workflow_call inputs, outputs and secrets.
    let call: Vec<(&str, &str)> = group(&gha, "Called with").iter().map(|l| (field(l, "kind"), field(l, "name"))).collect();
    assert_eq!(call, [("input", "toolchain"), ("output", "built"), ("secret", "TOKEN")], "{gha}");
    // Secrets and variables by name only.
    let names: Vec<&str> = group(&gha, "Secrets and variables").iter().map(|l| field(l, "name")).collect();
    assert_eq!(names, ["TOKEN", "REGION"]);
    assert!(gha.get("groups").is_some_and(|g| !g.to_string().contains("${{ secrets")), "no value of a secret, or its expression, in the groups");
    // Every verb is there, and each one off says why (W29-R04): a reusable workflow runs where it's
    // called, there's no draft to make ready or discard, and releases come later; the admin may
    // save a draft.
    let verbs = gha.pointer("/strip/verbs").and_then(Value::as_array).cloned().unwrap_or_default();
    assert_eq!(verbs.iter().map(|v| field(v, "verb")).collect::<Vec<_>>(), ["run", "save_draft", "ready", "discard", "release"]);
    assert!(verbs.iter().all(|v| v.get("enabled") == Some(&Value::Bool(true)) || !field(v, "why_not").is_empty()), "{verbs:?}");
    let on: Vec<&str> = verbs.iter().filter(|v| v.get("enabled") == Some(&Value::Bool(true))).map(|v| field(v, "verb")).collect();
    assert_eq!(on, ["save_draft"], "{verbs:?}");

    // Each action kind's own groups, and its versions.
    for (path, own) in [(".github/actions/setup/action.yml", "Steps"), (".github/actions/comment/action.yml", "Runs on Node (node24)"), (".github/actions/image/action.yml", "Image"), (".kiln/actions/fuzz/action.yml", "Firecracker microVM"), (".kiln/actions/sandbox/action.yml", "Shards microVM")] {
        let (st, p) = pane(&s, &s.admin, path, None)?;
        anyhow::ensure!(st == 200, "{path}'s pane: {st} {p}");
        assert_eq!(group_names(&p), ["Inputs", "Outputs", own, "Versions", "Used by", "Settings"], "{path}");
        let versions: Vec<&str> = group(&p, "Versions").iter().map(|l| field(l, "name")).collect();
        assert_eq!(versions, ["the default branch's tip", "v1.0.0"], "{path}");
    }
    let (_, fuzz) = pane(&s, &s.admin, ".kiln/actions/fuzz/action.yml", None)?;
    let vm: Vec<String> = group(&fuzz, "Firecracker microVM").iter().map(|l| format!("{}: {}", field(l, "name"), field(l, "value"))).collect();
    assert_eq!(vm, ["image: docker://alpine:3.20", "resources: cpus 2", "egress: crates.io"]);

    // At the tag, the file as it was then.
    let (st, old) = pane(&s, &s.admin, ".github/workflows/every.yml", Some("v1.0.0"))?;
    anyhow::ensure!(st == 200, "at v1.0.0: {st} {old}");
    assert_eq!((at(&old, "/head/title"), at(&old, "/head/commit"), at(&old, "/head/ref")), ("every event", tagged.as_str(), "v1.0.0"), "{old}");
    assert!(at(&old, "/detail/text").contains("name: every event\n"), "{old}");
    as_route(&s, "return await kiln.ship.pipelines.get({ repo: 'acme/kit', path: '.github/workflows/every.yml', ref: 'v1.0.0' })", "/api/ship/pipelines/acme/kit?path=.github/workflows/every.yml&ref=v1.0.0")?;

    // What isn't there, and a repository the caller may not read, are missing.
    assert_eq!(pane(&s, &s.admin, ".github/workflows/none.yml", None)?.0, 404);
    let (st, _) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/other", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "acme/other");
    let other = person(&s, "other", &["viewer@repo:acme/other"])?;
    assert_eq!(pane(&s, &other, ".github/workflows/every.yml", None)?.0, 404);
    Ok(())
}

/// What code mode's `program` returns, run by `token` over MCP: whether it failed, and its answer.
fn code_as(s: &Server, token: &str, program: &str) -> anyhow::Result<(bool, Value)> {
    let (st, body) = http(s.port, "POST", "/api/mcp", token, Some(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "code_run", "arguments": { "program": program } } })))?;
    anyhow::ensure!(st == 200, "code_run: {st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    let text = v.pointer("/result/content/0/text").and_then(Value::as_str).unwrap_or_default();
    let failed = v.pointer("/result/isError") == Some(&json!(true));
    Ok((failed, serde_json::from_str::<Value>(text).ok().and_then(|o| o.get("result").cloned()).unwrap_or_else(|| json!(text))))
}

/// A Pipelines verb through Ship as `token`: its status and answer.
fn act(s: &Server, token: &str, body: &Value) -> anyhow::Result<(u16, Value)> {
    let (st, text) = http(s.port, "POST", "/api/ship/pipelines", token, Some(body))?;
    Ok((st, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

/// The row of a draft in a repository's rows, if there is one, as the list shows `token`.
fn draft_row(s: &Server, token: &str, repo: &str, path: &str) -> anyhow::Result<Option<Value>> {
    let list = pipelines(s, token, &format!("?repo={repo}"))?;
    Ok(rows_of(&list, repo).into_iter().find(|r| field(r, "path") == path && r.get("draft").is_some_and(|d| !d.is_null())))
}

/// Until `pick` holds of what the list shows `token`, or a minute passes: the last it showed.
fn until_list(s: &Server, token: &str, what: &str, pick: impl Fn(&Value) -> bool) -> anyhow::Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let list = pipelines(s, token, "")?;
        if pick(&list) {
            return Ok(list);
        }
        anyhow::ensure!(Instant::now() < deadline, "{what}: {list}");
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// W29-R10 in Ship: a draft has a row of its own, DRAFT beside its title, that says Unpublished
/// to its author and, once it's ready, Review to someone else; its pane says the same and offers
/// its author's verbs. Its verbs go through `POST /api/ship/pipelines` and code mode's
/// `kiln.ship.pipelines.act` alike, each as its own operation: one without its right is refused,
/// and what a verb did is in the audit log. Discard takes the row away.
#[test]
fn w29_r10_drafts_in_ship() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "", extra: "" })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    git(&home, dir.path(), &["clone", "-q", &format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port), "app"])?;
    let work = dir.path().join("app");
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(work.join(".github/workflows/ci.yml"), "name: ci\non: push\njobs:\n  build:\n    runs-on: local\n    steps:\n      - run: echo ci\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;
    until_list(&s, &s.admin, "acme/app's rows", |l| !rows_of(l, "acme/app").is_empty())?;
    let reviewer = person(&s, "rae", &["viewer@repo:acme/app"])?;

    // Save draft: a row of its own, Unpublished.
    let path = ".github/workflows/try.yml";
    let text = "name: try\non: [push, workflow_dispatch]\njobs:\n  t:\n    runs-on: local\n    steps:\n      - run: echo tried\n";
    let (st, saved) = act(&s, &s.admin, &json!({ "repo": "acme/app", "path": path, "verb": "save_draft", "content": text, "title": "Try it" }))?;
    assert_eq!(st, 200, "{saved}");
    let id = saved.pointer("/draft/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    until_list(&s, &s.admin, "the draft's row", |l| rows_of(l, "acme/app").iter().any(|r| field(r, "path") == path && r.pointer("/draft/id").and_then(Value::as_str) == Some(id.as_str())))?;
    let mine = draft_row(&s, &s.admin, "acme/app", path)?.unwrap_or_default();
    assert_eq!((field(&mine, "word"), field(&mine, "stand"), mine.pointer("/draft/state").and_then(Value::as_str)), ("Unpublished", "waiting", Some("unpublished")), "{mine}");
    assert!(field(&mine, "key").contains(":draft:"), "{mine}");
    assert!(rows_of(&pipelines(&s, &s.admin, "?repo=acme/app")?, "acme/app").iter().any(|r| field(r, "path") == ".github/workflows/ci.yml" && r.get("draft").is_some_and(Value::is_null)), "what's published stays");
    // Its pane, for its author: the DRAFT mark, and Ready, Discard and Run.
    let (st, pane) = http(s.port, "GET", &format!("/api/ship/pipelines/acme/app?path={path}&draft={id}"), &s.admin, None)?;
    anyhow::ensure!(st == 200, "{st} {pane}");
    let pane: Value = serde_json::from_str(&pane)?;
    assert_eq!(pane.pointer("/head/draft/id").and_then(Value::as_str), Some(id.as_str()), "{pane}");
    let on: Vec<String> = pane.pointer("/strip/verbs").and_then(Value::as_array).into_iter().flatten().filter(|v| v.get("enabled") == Some(&json!(true))).map(|v| field(v, "verb").to_owned()).collect();
    assert_eq!(on, ["run", "save_draft", "ready", "discard"], "{pane}");

    // Run: the draft as its author's test.
    let (st, ran) = act(&s, &s.admin, &json!({ "repo": "acme/app", "path": path, "verb": "run", "event": "push" }))?;
    assert_eq!(st, 200, "{ran}");
    assert!(ran.pointer("/runs/0/id").is_some(), "{ran}");

    // Ready: Review to someone else, Unpublished still to its author.
    let (st, out) = act(&s, &s.admin, &json!({ "repo": "acme/app", "path": path, "verb": "ready" }))?;
    assert_eq!(st, 200, "{out}");
    let theirs = until_list(&s, &reviewer, "Review, to someone else", |l| rows_of(l, "acme/app").iter().any(|r| field(r, "path") == path && field(r, "word") == "Review"))?;
    let row = rows_of(&theirs, "acme/app").into_iter().find(|r| field(r, "path") == path && !r.get("draft").is_some_and(Value::is_null)).unwrap_or_default();
    assert_eq!(field(&row, "stand"), "needs_you", "{row}");
    let mine = draft_row(&s, &s.admin, "acme/app", path)?.unwrap_or_default();
    assert_eq!(field(&mine, "word"), "Unpublished", "to its author: {mine}");
    // Someone without its right is refused, as the operation would refuse them.
    let (st, said) = act(&s, &reviewer, &json!({ "repo": "acme/app", "path": path, "verb": "discard" }))?;
    assert_eq!(st, 403, "{said}");

    // Code mode's act answers as the route: a second draft, through it.
    let program = "return await kiln.ship.pipelines.act({ repo: 'acme/app', path: '.github/workflows/other.yml', verb: 'save_draft', content: 'name: other\\non: push\\njobs:\\n  o:\\n    runs-on: local\\n    steps:\\n      - run: echo other\\n', title: 'Another' })";
    let (failed, out) = code_as(&s, &s.admin, program)?;
    assert!(!failed, "{out}");
    assert_eq!((out.pointer("/draft/kind").and_then(Value::as_str), out.pointer("/draft/state").and_then(Value::as_str), out.pointer("/draft/path").and_then(Value::as_str)), (Some("diff"), Some("draft"), Some(".github/workflows/other.yml")), "{out}");

    // The audit log has each verb's request, what its operation did, and the refusal (W29-AC04).
    let (_, audit) = http(s.port, "GET", "/api/audit?limit=200", &s.admin, None)?;
    let entries: Vec<Value> = serde_json::from_str::<Value>(&audit)?.as_array().cloned().unwrap_or_default();
    let ours: Vec<&Value> = entries.iter().filter(|e| field(e, "action") == "POST /api/ship/pipelines").collect();
    for what in ["ci.workflow.draft:", "diffs.ready:", "ci.workflow.draft.ready:", "ci.workflow.draft.run:"] {
        assert!(ours.iter().any(|e| field(e, "detail").contains(what)), "{what} in the audit log: {audit}");
    }
    assert!(ours.iter().any(|e| field(e, "result") == "403" && field(e, "on_behalf_of") == "user:rae"), "the refusal in the audit log: {audit}");

    // Discard: the row goes.
    let (st, out) = act(&s, &s.admin, &json!({ "repo": "acme/app", "path": path, "verb": "discard" }))?;
    assert_eq!(st, 200, "{out}");
    until_list(&s, &s.admin, "the discarded draft's row gone", |l| !rows_of(l, "acme/app").iter().any(|r| r.pointer("/draft/id").and_then(Value::as_str) == Some(id.as_str())))?;
    Ok(())
}
