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
    // W29-R02, pl-1: changing one file moves its row's commit alone. A change in an action's
    // helper file moves that action, while another workflow keeps the commit that last changed it.
    std::fs::write(work.join(".github/workflows/ci.yml"), "name: changed\non: push\njobs:\n  a:\n    runs-on: local\n    steps: [{ run: echo changed }]\n")?;
    std::fs::write(work.join(".github/actions/setup/run.sh"), "echo changed\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "W29-R02: two files changed"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;
    let changed = git(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();
    let list = until_list(&s, &s.admin, "each file's commit", |v| field(row(&rows_of(v, "acme/app"), ".github/workflows/ci.yml").unwrap_or(&Value::Null), "commit") == changed)?;
    let rows = rows_of(&list, "acme/app");
    for path in [".github/workflows/ci.yml", ".github/actions/setup/action.yml"] {
        assert_eq!(field(row(&rows, path).unwrap_or(&Value::Null), "commit"), changed, "{list}");
    }
    assert_eq!(field(row(&rows, ".kiln/ci.yaml").unwrap_or(&Value::Null), "commit"), head, "{list}");

    // W29-R29, R30: a GitHub workflow that uses this repository's microVM actions is warned at each
    // of their steps (`KCI5003`): GitHub's runner can't run them. Its other actions are no matter.
    let gha = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - uses: ./.kiln/actions/fuzz\n      - uses: ./.kiln/actions/sandbox\n      - uses: ./.github/actions/setup\n";
    let (st, body) = http(s.port, "POST", "/api/ci/validate", &s.admin, Some(&json!({ "path": ".github/workflows/vm.yml", "content": gha, "repo": "acme/app" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let checked: Value = serde_json::from_str(&body)?;
    let warned: Vec<(u64, String)> = checked
        .get("diagnostics")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|d| d.get("code") == Some(&json!("KCI5003")))
        .map(|d| (d.pointer("/range/start/line").and_then(Value::as_u64).unwrap_or_default(), field(d, "severity").to_owned()))
        .collect();
    assert_eq!(warned, vec![(7, "warning".to_owned()), (8, "warning".to_owned())], "{body}");
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
    let called = "on: workflow_call\njobs:\n  build:\n    runs-on: local\n    steps:\n      - uses: actions/checkout@v4\n      - run: echo checked\n";
    files.push((".github/workflows/called.yml", called));
    files.push((".kiln/caller.yaml", "name: caller\non: manual\njobs:\n  rust:\n    uses: kiln://acme/kit/.github/workflows/called.yml@v1.0.0\n"));
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
    std::fs::write(work.join(".github/workflows/called.yml"), called.replacen("  build:\n", "  next:\n", 1))?;
    git(&home, &work, &["commit", "-qam", "renamed"])?;
    git(&home, &work, &["push", "-q", "origin", "main", "v1.0.0"])?;
    let head = git(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();

    // W29-R04: the action mirror already has a floating tag. A later release at a different
    // commit must not be mistaken for what this workflow's v4 resolves to. Opening the pane
    // reads these local facts without fetching the action.
    git(&home, &work, &["tag", "v4", &tagged])?;
    git(&home, &work, &["tag", "v4.2.3", &tagged])?;
    git(&home, &work, &["tag", "v4.2.4", &head])?;
    let mirrors = dir.path().join("data/actions");
    std::fs::create_dir_all(&mirrors)?;
    git(&home, &mirrors, &["clone", "-q", "--bare", &work.to_string_lossy(), "actions__checkout.git"])?;

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
    let checkout = group(&gha, "Uses").into_iter().find(|l| field(l, "name") == "actions/checkout@v4");
    assert_eq!(checkout.map(|l| field(l, "value")), Some(format!("fetched by name · 4.2.3 · {}", tagged.get(..7).unwrap_or_default()).as_str()), "{gha}");
    // The exact cached tag's job and dependency, without a run or a network fetch (pl-7).
    let (st, caller) = pane(&s, &s.admin, ".kiln/caller.yaml", None)?;
    anyhow::ensure!(st == 200, "the caller's pane: {st} {caller}");
    assert_eq!(group(&caller, "Jobs").first().map(|l| field(l, "name")), Some("rust / build"), "{caller}");
    assert!(group(&caller, "Uses").iter().any(|l| field(l, "name") == "actions/checkout@v4"), "{caller}");
    // A reusable workflow's interface: its workflow_call inputs, outputs and secrets.
    let call: Vec<(&str, &str)> = group(&gha, "Called with").iter().map(|l| (field(l, "kind"), field(l, "name"))).collect();
    assert_eq!(call, [("input", "toolchain"), ("output", "built"), ("secret", "TOKEN")], "{gha}");
    // Secrets and variables by name only.
    let names: Vec<&str> = group(&gha, "Secrets and variables").iter().map(|l| field(l, "name")).collect();
    assert_eq!(names, ["TOKEN", "REGION"]);
    assert!(gha.get("groups").is_some_and(|g| !g.to_string().contains("${{ secrets")), "no value of a secret, or its expression, in the groups");
    // Every verb is there, and each one off says why (W29-R04): a reusable workflow runs where it's
    // called, and there's no draft to make ready or discard; the admin may save a draft, and
    // Release (W29-R21).
    let verbs = gha.pointer("/strip/verbs").and_then(Value::as_array).cloned().unwrap_or_default();
    assert_eq!(verbs.iter().map(|v| field(v, "verb")).collect::<Vec<_>>(), ["run", "save_draft", "ready", "discard", "release"]);
    assert!(verbs.iter().all(|v| v.get("enabled") == Some(&Value::Bool(true)) || !field(v, "why_not").is_empty()), "{verbs:?}");
    let on: Vec<&str> = verbs.iter().filter(|v| v.get("enabled") == Some(&Value::Bool(true))).map(|v| field(v, "verb")).collect();
    assert_eq!(on, ["save_draft", "release"], "{verbs:?}");

    // Each action kind's own groups, and its versions.
    for (path, own) in [(".github/actions/setup/action.yml", "Steps"), (".github/actions/comment/action.yml", "Runs on Node (node24)"), (".github/actions/image/action.yml", "Image"), (".kiln/actions/fuzz/action.yml", "Firecracker microVM"), (".kiln/actions/sandbox/action.yml", "Shards microVM")] {
        let (st, p) = pane(&s, &s.admin, path, None)?;
        anyhow::ensure!(st == 200, "{path}'s pane: {st} {p}");
        // Outputs only where it has some (pl-5); the tip by its commit.
        let want: Vec<&str> = if path.contains("/setup/") { vec!["Inputs", "Outputs", own, "Versions", "Used by", "Settings"] } else { vec!["Inputs", own, "Versions", "Used by", "Settings"] };
        assert_eq!(group_names(&p), want, "{path}");
        let versions: Vec<&str> = group(&p, "Versions").iter().map(|l| field(l, "name")).collect();
        assert_eq!(versions, [head.get(..7).unwrap_or_default(), "v1.0.0"], "{path}");
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
    // A draft of what's published: the published pane's `changed in a draft` keeps what it changes (R04, pl-2).
    let ci = ".github/workflows/ci.yml";
    let changed = "name: ci\non: push\njobs:\n  build:\n    runs-on: local\n    steps:\n      - run: echo changed\n";
    let (st, saved) = act(&s, &s.admin, &json!({ "repo": "acme/app", "path": ci, "verb": "save_draft", "content": changed, "title": "Change ci" }))?;
    assert_eq!(st, 200, "{saved}");
    let (st, published) = http(s.port, "GET", &format!("/api/ship/pipelines/acme/app?path={ci}"), &s.admin, None)?;
    anyhow::ensure!(st == 200, "{st} {published}");
    let published: Value = serde_json::from_str(&published)?;
    assert_eq!(published.pointer("/strip/pick"), Some(&json!(["failed last run", "changed in a draft", "all"])), "{published}");
    assert_eq!(published.pointer("/strip/narrow/changed in a draft"), Some(&json!(["jobs.build"])), "{published}");
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

/// A repository kiln hosts, cloned under `dir` (its `/` a `_`), its first commit (a README and
/// `files`, tagged `tag`) pushed and polled.
fn hosted(s: &Server, home: &Path, dir: &Path, name: &str, files: &[(&str, &str)], tag: Option<&str>) -> anyhow::Result<std::path::PathBuf> {
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": name, "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{name}: {st} {body}");
    let leaf = name.replace('/', "_");
    git(home, dir, &["clone", "-q", &format!("http://x:{}@127.0.0.1:{}/git/{name}.git", s.admin, s.port), &leaf])?;
    let work = dir.join(&leaf);
    let mut all: Vec<(&str, &str)> = vec![("README.md", "first\n")];
    all.extend_from_slice(files);
    for (path, text) in all {
        let p = work.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, text)?;
    }
    git(home, &work, &["add", "."])?;
    git(home, &work, &["commit", "-qm", "first"])?;
    let mut refs = vec!["push", "-q", "origin", "main"];
    if let Some(t) = tag {
        git(home, &work, &["tag", t])?;
        refs.push(t);
    }
    git(home, &work, &refs)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let list = v.as_array().cloned().or_else(|| v.get("repos").and_then(Value::as_array).cloned()).unwrap_or_default();
        if list.iter().any(|r| field(r, "name") == name && r.get("last_polled_at").is_some_and(|t| !t.is_null())) {
            return Ok(work);
        }
        anyhow::ensure!(Instant::now() < deadline, "{name} wasn't polled: {body}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// `repo`'s runs at `sha`, once `n` of them have finished.
fn finished(s: &Server, repo: &str, sha: &str, n: usize) -> anyhow::Result<Vec<Value>> {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let (_, body) = http(s.port, "GET", &format!("/api/ci/runs?repo={repo}&limit=100"), &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let all: Vec<Value> = v.get("runs").and_then(Value::as_array).into_iter().flatten().filter(|r| field(r, "sha") == sha).cloned().collect();
        let done = all.iter().filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())).count();
        if done >= n {
            return Ok(all);
        }
        anyhow::ensure!(Instant::now() < deadline, "{repo} at {sha}: {done} of {n} runs finished: {all:?}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// W29-R17, R20 in Ship: a shared repository's list says its access, and its heading that it's
/// shared; an action's pane lists the repositories whose runs used it, with the version, and its
/// Access in Settings, and a reusable workflow's pane too. One that shares nothing says none.
#[test]
fn w29_r17_access_and_used_by_in_ship() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: "" })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let setup = "name: setup\nruns:\n  using: composite\n  steps:\n    - run: echo set up\n      shell: bash\n";
    let build = "name: build\non:\n  workflow_call:\njobs:\n  b:\n    runs-on: local\n    steps:\n      - run: echo built\n";
    // acme/kit shares with acme's repositories, from its trunk's first commit, tagged v1.0.0.
    hosted(&s, &home, dir.path(), "acme/kit", &[(".kiln/config.yaml", "share: owner\n"), (".github/actions/setup/action.yml", setup), (".github/workflows/build.yml", build)], Some("v1.0.0"))?;
    let app = hosted(&s, &home, dir.path(), "acme/app", &[], None)?;
    let step = "on: push\njobs:\n  a:\n    runs-on: local\n    steps:\n      - uses: kiln://acme/kit/.github/actions/setup@v1.0.0\n";
    let call = "on: push\njobs:\n  c:\n    uses: kiln://acme/kit/.github/workflows/build.yml@v1.0.0\n";
    for (path, text) in [(".kiln/step.yaml", step), (".kiln/call.yaml", call)] {
        let p = app.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, text)?;
    }
    git(&home, &app, &["add", "."])?;
    git(&home, &app, &["commit", "-qm", "uses acme/kit"])?;
    git(&home, &app, &["push", "-q", "origin", "main"])?;
    let sha = git(&home, &app, &["rev-parse", "HEAD"])?.trim().to_owned();
    let runs = finished(&s, "acme/app", &sha, 2)?;
    assert!(runs.iter().all(|r| field(r, "conclusion") == "success"), "{runs:?}\n{}", s.log());

    // The list: acme/kit shared with its owner's repositories, acme/app not shared.
    let list = pipelines(&s, &s.admin, "")?;
    let head = |repo: &str| list.get("repos").and_then(Value::as_array).and_then(|r| r.iter().find(|r| field(r, "repo") == repo)).cloned().unwrap_or_default();
    let (kit, mine) = (head("acme/kit"), head("acme/app"));
    assert_eq!((field(&kit, "access"), field(&mine, "access")), ("owner", "none"), "{list}");
    assert!(field(&kit, "say").starts_with("shared"), "{kit}");
    assert!(!field(&mine, "say").contains("shared"), "{mine}");

    // The action's pane: who used which version, and its Access.
    let (st, action) = pane(&s, &s.admin, ".github/actions/setup/action.yml", None)?;
    anyhow::ensure!(st == 200, "the action's pane: {st} {action}");
    let used: Vec<(&str, &str)> = group(&action, "Used by").iter().map(|l| (field(l, "name"), field(l, "value"))).collect();
    assert_eq!(used.len(), 1, "{action}");
    assert!(used.first().is_some_and(|(repo, v)| *repo == "acme/app" && v.ends_with("v1.0.0")), "{used:?}");
    let access: Vec<(&str, &str, &str)> = group(&action, "Settings").iter().filter(|l| field(l, "key") == "settings.access").map(|l| (field(l, "name"), field(l, "value"), field(l, "state"))).collect();
    assert_eq!(access, [("Access", "shared with acme's repositories", "owner")], "{action}");

    // The reusable workflow's pane, likewise.
    let (st, called) = pane(&s, &s.admin, ".github/workflows/build.yml", None)?;
    anyhow::ensure!(st == 200, "the reusable workflow's pane: {st} {called}");
    let names = group_names(&called);
    assert_eq!(names.iter().rev().take(2).map(String::as_str).collect::<Vec<_>>(), ["Settings", "Used by"], "{names:?}");
    assert_eq!(group(&called, "Used by").iter().map(|l| field(l, "name")).collect::<Vec<_>>(), ["acme/app"], "{called}");
    assert!(group(&called, "Settings").iter().any(|l| field(l, "key") == "settings.access" && field(l, "state") == "owner"), "{called}");
    Ok(())
}

/// W29-R21, R22 in Ship: an action's Release asks Keep · Release, saying what releasing its head
/// changes for the workflows that use it. Once it's released, a consumer that asked for updates
/// has an update draft: an Update row whose pane says what changes for it and what changed
/// upstream.
#[test]
fn w29_r21_r22_release_and_updates_in_ship() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "", extra: "" })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let v1 = "name: setup\ninputs:\n  who:\n    required: true\nruns:\n  using: composite\n  steps:\n    - run: echo set up\n      shell: bash\n";
    let kit = hosted(&s, &home, dir.path(), "acme/kit", &[(".kiln/config.yaml", "share: owner\n"), (".github/actions/setup/action.yml", v1)], None)?;
    let release = |sha: &str, tag: &str| -> anyhow::Result<()> {
        let (st, out) = http(s.port, "POST", "/api/ci/repos/acme%2Fkit/releases", &s.admin, Some(&json!({ "sha": sha, "tag": tag })))?;
        anyhow::ensure!(st == 200, "{tag}: {st} {out}");
        Ok(())
    };
    release(git(&home, &kit, &["rev-parse", "HEAD"])?.trim(), "v1.0.0")?;
    let app = hosted(&s, &home, dir.path(), "acme/app", &[(".kiln/config.yaml", "updates:\n  actions: on\n  cooldown: 0\n")], None)?;
    let ci = "name: ci\non: push\njobs:\n  a:\n    runs-on: local\n    steps:\n      - uses: kiln://acme/kit/.github/actions/setup@v1.0.0\n        with:\n          who: me\n";
    std::fs::create_dir_all(app.join(".github/workflows"))?;
    std::fs::write(app.join(".github/workflows/ci.yml"), ci)?;
    git(&home, &app, &["add", "."])?;
    git(&home, &app, &["commit", "-qm", "uses acme/kit"])?;
    git(&home, &app, &["push", "-q", "origin", "main"])?;
    let sha = git(&home, &app, &["rev-parse", "HEAD"])?.trim().to_owned();
    let runs = finished(&s, "acme/app", &sha, 1)?;
    assert!(runs.iter().all(|r| field(r, "conclusion") == "success"), "{runs:?}\n{}", s.log());

    // The head asks for a new input: Release says so before it releases.
    let v2 = v1.replace("inputs:\n", "inputs:\n  where:\n    required: true\n");
    std::fs::write(kit.join(".github/actions/setup/action.yml"), &v2)?;
    git(&home, &kit, &["commit", "-qam", "where"])?;
    git(&home, &kit, &["push", "-q", "origin", "main"])?;
    let head = git(&home, &kit, &["rev-parse", "HEAD"])?.trim().to_owned();
    // The pane reads the repository as kiln's mirror of it has it: until it has the push.
    let deadline = Instant::now() + Duration::from_secs(60);
    let p = loop {
        let (st, p) = pane(&s, &s.admin, ".github/actions/setup/action.yml", None)?;
        anyhow::ensure!(st == 200, "the action's pane: {st} {p}");
        if at(&p, "/head/commit") == head {
            break p;
        }
        anyhow::ensure!(Instant::now() < deadline, "the pane never read {head}: {p}");
        std::thread::sleep(Duration::from_millis(300));
    };
    let verbs = p.pointer("/strip/verbs").and_then(Value::as_array).cloned().unwrap_or_default();
    let asks = verbs.iter().find(|v| field(v, "verb") == "release").map(|v| field(v, "asks").to_owned()).unwrap_or_default();
    assert!(asks.starts_with("Keep · Release") && asks.contains("input `where` added, required"), "{asks:?} in {verbs:?}");
    // Its Changes says it in a sentence, marked, then its repository's own checks on its head (pl-5).
    let lines: Vec<(&str, &str)> = p.pointer("/detail/changes").and_then(Value::as_array).into_iter().flatten().map(|l| (field(l, "name"), field(l, "value"))).collect();
    assert_eq!(lines.first().copied(), Some(("a new input, where (required): each use has to pass it", "breaks")), "{p}");
    assert!(lines.last().is_some_and(|(n, _)| n.starts_with("kit's own checks ") && n.ends_with(head.get(..7).unwrap_or_default())), "{lines:?}");

    // Released, acme/app's update draft says what changes for it.
    release(git(&home, &kit, &["rev-parse", "HEAD"])?.trim(), "v1.1.0")?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let id = loop {
        let list = pipelines(&s, &s.admin, "?repo=acme/app")?;
        let rows = rows_of(&list, "acme/app");
        if let Some(r) = rows.iter().find(|r| field(r, "path") == ".github/workflows/ci.yml" && at(r, "/draft/author").starts_with("kiln-updates:")) {
            break at(r, "/draft/id").to_owned();
        }
        anyhow::ensure!(Instant::now() < deadline, "no update draft in acme/app: {list}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let (st, body) = http(s.port, "GET", &format!("/api/ship/pipelines/acme/app?path=.github/workflows/ci.yml&draft={id}"), &s.admin, None)?;
    anyhow::ensure!(st == 200, "the update draft's pane: {st} {body}");
    let p: Value = serde_json::from_str(&body)?;
    // W29-R25: the consumer update carries complete locks in that same draft revision.
    let (st, body) = http(s.port, "GET", &format!("/api/ci/repos/acme%2Fapp/locks/verify?sha={}", at(&p, "/head/commit")), &s.admin, None)?;
    let verified: Value = serde_json::from_str(&body)?;
    assert_eq!((st, verified.get("fresh")), (200, Some(&json!(true))), "{verified}");
    // Its ordinary groups (pl-7): the use it moves in Uses, under the version it had, marked with where it goes.
    let names = group_names(&p);
    assert!(!names.iter().any(|n| n == "What changes for it" || n == "Upstream"), "{names:?}");
    let moved: Vec<(&str, &str, &str)> = group(&p, "Uses").iter().filter(|l| field(l, "state") == "changes").map(|l| (field(l, "key"), field(l, "name"), field(l, "value"))).collect();
    assert_eq!(moved.len(), 1, "{p}");
    assert!(moved.iter().all(|(k, n, v)| k.ends_with("/.github/actions/setup@v1.1.0") && n.ends_with("/.github/actions/setup@v1.0.0") && v.contains(" · → v1.1.0")), "{moved:?}");
    // Its strip says what's out, the commits since, and what it changes in short (pl-7); its
    // Changes, in sentences.
    let said: Vec<&str> = p.pointer("/strip/said").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    assert_eq!(said.get(..3), Some(&["setup v1.1.0 is out", "1 commit since v1.0.0", "a new required input; no new secrets, permissions or egress"][..]), "{p}");
    assert_eq!(p.pointer("/strip/strong/0").and_then(Value::as_str), Some("setup v1.1.0"), "{p}");
    let lines: Vec<(&str, &str)> = p.pointer("/detail/changes").and_then(Value::as_array).into_iter().flatten().map(|l| (field(l, "kind"), field(l, "name"))).collect();
    assert_eq!(lines.get(..2), Some(&[("change", "a new input, where (required): app has to pass it"), ("note", "no new secrets, permissions or egress")][..]), "{p}");
    assert_eq!(p.pointer("/detail/update"), Some(&json!({ "uses": "setup", "from": "v1.0.0", "to": "v1.1.0" })), "{p}");
    // And the upstream code's diff between the two versions (W29-R22), last.
    let upstream: Vec<&str> = lines.iter().filter(|(k, _)| *k == "upstream").map(|(_, n)| *n).collect();
    assert_eq!(upstream, ["acme/kit/.github/actions/setup v1.0.0 → v1.1.0"], "{p}");
    assert!(at(&p, "/detail/text").contains("setup@v1.1.0"), "{p}");
    Ok(())
}

/// W29-R06's Times view: a workflow's preview carries each schedule's options and its next three
/// runs, read as W03's cron reads them (its time zone, daylight saving and all), as
/// `kiln_ci::trigger::cron::next_runs` makes them at the moment it's asked.
#[test]
fn w29_r06_schedule_times_in_the_preview() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: false, ci: "kiln_workflows = true", extra: "" })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let work = hosted(&s, &home, dir.path(), "acme/sched", &[], None)?;
    // Its commit named: the preview reads refs through the mirror, which catches up after a push.
    let sha = git(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();
    let content = "name: smoke\non:\n  schedule:\n    - cron: \"0 3 * * *\"\n      timezone: America/Los_Angeles\n      skip-if-unchanged: true\n      inputs: { target: production }\n    - cron: \"*/30 * * * *\"\njobs:\n  smoke:\n    runs-on: local\n    steps:\n      - run: echo smoke\n";
    let unix = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX)).unwrap_or_default();
    let before = unix();
    let (st, body) = http(s.port, "POST", "/api/ci/plan-preview", &s.admin, Some(&json!({ "repo": "acme/sched", "ref": "main", "sha": sha, "path": ".kiln/smoke.yaml", "content": content, "event": "schedule" })))?;
    let after = unix();
    anyhow::ensure!(st == 200, "{st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    let schedules = v.get("schedules").and_then(Value::as_array).cloned().unwrap_or_default();
    let (Some(first), Some(second), 2) = (schedules.first(), schedules.get(1), schedules.len()) else { anyhow::bail!("both schedules: {v}") };
    for (s, cron, tz) in [(first, "0 3 * * *", Some("America/Los_Angeles")), (second, "*/30 * * * *", None)] {
        assert_eq!(field(s, "cron"), cron);
        assert_eq!(s.get("timezone").and_then(Value::as_str), tz);
        let want = |now: i64| serde_json::to_value(kiln_ci::trigger::cron::next_runs(cron, tz, now, 3).unwrap_or_default()).unwrap_or_default();
        let next = s.get("next").cloned().unwrap_or_default();
        assert!(next == want(before) || next == want(after), "{cron}: {next} is next_runs' at {before} or {after}");
        assert_eq!(next.as_array().map(Vec::len), Some(3), "{s}");
    }
    assert_eq!(first.get("skip_if_unchanged"), Some(&json!(true)));
    assert_eq!(first.pointer("/inputs/target").and_then(Value::as_str), Some("production"));
    Ok(())
}
