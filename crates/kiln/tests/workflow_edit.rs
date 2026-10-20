//! W02-R12 and W21-R06 (docs/ci.md): a workflow checked against its repository, and saved from
//! its editor, against a server of its own with real `git`. Validating with `repo` adds what only
//! the server knows: a `runs-on` label no runner, routing rule or pool has (`KCI6007`), and the
//! uses the repository's action policy refuses (`KCI4021`, in place of the pin hint). Saving
//! (`POST /api/ci/repos/:repo/workflows/:path/edit`) checks first, then gives a patch, a diff
//! through review, or a commit to a branch that a push of it would be refused or taken like: the
//! trunk is the land queue's, a branch is the developer's, and its push starts the workflow.

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
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// A user with `grants`, and a personal access token that may do what they may.
fn person(s: &Server, name: &str, grants: &[&str]) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": grants })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}'s token: {st} {body}");
    serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))
}

fn post(s: &Server, token: &str, path: &str, body: &Value) -> anyhow::Result<(u16, Value)> {
    let (st, text) = http(s.port, "POST", path, token, Some(body))?;
    Ok((st, serde_json::from_str(&text).unwrap_or(json!({ "text": text }))))
}

/// The diagnostics with `code`, as (line, col).
fn at(v: &Value, code: &str) -> Vec<(u64, u64)> {
    let all = v.get("diagnostics").and_then(Value::as_array).cloned().unwrap_or_default();
    all.iter()
        .filter(|d| d.get("code").and_then(Value::as_str) == Some(code))
        .map(|d| (d.pointer("/range/start/line").and_then(Value::as_u64).unwrap_or(0), d.pointer("/range/start/col").and_then(Value::as_u64).unwrap_or(0)))
        .collect()
}

const CI: &str = "on: push\njobs:\n  build:\n    runs-on: local\n    steps:\n      - run: echo built\n";

#[test]
fn w02_r12_checks_and_saves_a_workflow() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // `big` is mapped by a routing rule; nothing has `gpu-box`.
    let extra = "[[ci.routing]]\nmatch = { labels = [\"big\"] }\nrequire = [\"cpus >= 1\"]\n";
    let s = start(dir.path(), &Options { agent: true, extra, ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
    let viewer = person(&s, "viewer", &["viewer@repo:acme/app"])?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    git(&home, dir.path(), &["clone", "-q", &url(&dev), "dev"])?;
    let work = dir.path().join("dev");
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(work.join(".github/workflows/ci.yml"), CI)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;

    // The runner (`local`) is in: its labels are known.
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let (_, body) = http(s.port, "GET", "/api/agents", &s.admin, None)?;
        if body.contains("\"local\"") {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "no runner: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    // What a `runs-on` may name here, each label with where it comes from.
    let (st, body) = http(s.port, "GET", "/api/ci/repos/acme%2Fapp/labels", &dev, None)?;
    let known: Value = serde_json::from_str(&body)?;
    let from = |label: &str| known.get("labels").and_then(Value::as_array).and_then(|l| l.iter().find(|x| x.get("label") == Some(&json!(label)))).and_then(|x| x.get("from")).cloned();
    assert!(st == 200 && from("local") == Some(json!(["runner"])) && from("self-hosted") == Some(json!(["runner"])) && from("big") == Some(json!(["rule"])), "{st} {body}");
    // Checked alone, a file knows nothing of runners; for its repository, it does.
    let labels = "on: push\njobs:\n  a:\n    runs-on: local\n    steps: [{ run: 'true' }]\n  b:\n    runs-on: [self-hosted, linux]\n    steps: [{ run: 'true' }]\n  c:\n    runs-on: [local, gpu-box]\n    steps: [{ run: 'true' }]\n  d:\n    runs-on: big\n    steps: [{ run: 'true' }]\n  e:\n    strategy: { matrix: { os: [x, y] } }\n    runs-on: ${{ matrix.os }}\n    steps: [{ run: 'true' }]\n";
    let path = ".github/workflows/labels.yml";
    let (st, alone) = post(&s, &dev, "/api/ci/validate", &json!({ "path": path, "content": labels }))?;
    assert!(st == 200 && at(&alone, "KCI6007").is_empty(), "{st} {alone}");
    let (st, there) = post(&s, &dev, "/api/ci/validate", &json!({ "path": path, "content": labels, "repo": "acme/app" }))?;
    assert_eq!((st, at(&there, "KCI6007")), (200, vec![(10, 22)]), "only gpu-box, where it is: {there}");
    let message =
        there.get("diagnostics").and_then(Value::as_array).and_then(|l| l.iter().find(|d| d.get("code") == Some(&json!("KCI6007")))).and_then(|d| d.get("message")).cloned().unwrap_or_default();
    assert!(message.as_str().is_some_and(|m| m.contains("`gpu-box`") && m.contains("waits")), "{message}");
    // A repository the caller can't read is none to them.
    let other = person(&s, "other", &["developer@repo:acme/elsewhere"])?;
    let (st, _) = post(&s, &other, "/api/ci/validate", &json!({ "path": path, "content": labels, "repo": "acme/app" }))?;
    assert_eq!(st, 404);

    // The repository's policy requires pins: the tag is refused where it is, and isn't hinted.
    let pinned = "on: push\njobs:\n  build:\n    runs-on: local\n    steps:\n      - uses: actions/checkout@v4\n      - uses: actions/setup-node@1d0ff469b7ec7b3cb9d8673fde0c81c44821de2a\n";
    let (st, _) = http(s.port, "PUT", "/api/ci/actions/policy?repo=acme/app", &s.admin, Some(&json!({ "require_sha_pins": true })))?;
    assert_eq!(st, 200);
    let (_, alone) = post(&s, &dev, "/api/ci/validate", &json!({ "path": path, "content": pinned }))?;
    assert!(at(&alone, "KCI6002") == vec![(6, 9)] && at(&alone, "KCI4021").is_empty(), "{alone}");
    let (_, there) = post(&s, &dev, "/api/ci/validate", &json!({ "path": path, "content": pinned, "repo": "acme/app" }))?;
    assert!(at(&there, "KCI4021") == vec![(6, 9)] && at(&there, "KCI6002").is_empty(), "{there}");
    let (st, _) = http(s.port, "DELETE", "/api/ci/actions/policy?repo=acme/app", &s.admin, None)?;
    assert_eq!(st, 200);

    let edit = "/api/ci/repos/acme%2Fapp/workflows/.github/workflows/ci.yml/edit";
    let changed = CI.replace("echo built", "echo built, again");
    // Saving needs `workflows.edit`: a viewer reads, and saves nothing.
    let (st, v) = post(&s, &viewer, edit, &json!({ "content": changed, "to": "patch" }))?;
    assert_eq!(st, 403, "{v}");
    // A patch, against the default branch: nothing written.
    let (st, v) = post(&s, &dev, edit, &json!({ "content": changed, "to": "patch" }))?;
    let patch = v.get("patch").and_then(Value::as_str).unwrap_or_default();
    assert!(
        st == 200 && patch.contains("--- a/.github/workflows/ci.yml\n+++ b/.github/workflows/ci.yml\n") && patch.contains("-      - run: echo built\n+      - run: echo built, again\n"),
        "{st} {v}"
    );
    assert_eq!(v.get("to"), Some(&json!("patch")));
    // Errors refuse it, said.
    let (st, v) = post(&s, &dev, edit, &json!({ "content": "on: push\njobs:\n  build:\n    steps: []\n" }))?;
    assert!(st == 422 && v.to_string().contains("has errors"), "{st} {v}");
    // The trunk is the land queue's, as for a push.
    let (st, v) = post(&s, &dev, edit, &json!({ "content": changed, "to": "branch", "branch": "main" }))?;
    assert!(st == 403 && v.to_string().contains("main is a trunk"), "{st} {v}");
    // A branch is the developer's: a commit on the default branch's head, and the push's run.
    let (st, v) = post(&s, &dev, edit, &json!({ "content": changed, "to": "branch", "branch": "feature/edit", "title": "Say it again" }))?;
    assert!(st == 200 && v.get("to") == Some(&json!("branch")) && v.get("branch") == Some(&json!("feature/edit")), "{st} {v}");
    let sha = v.get("sha").and_then(Value::as_str).unwrap_or_default().to_owned();
    assert!(v.get("diagnostics").is_some_and(Value::is_array) && v.get("preview").is_some_and(Value::is_object), "{v}");
    let refs = git(&home, dir.path(), &["ls-remote", &url(&dev)])?;
    assert!(refs.lines().any(|l| l == format!("{sha}\trefs/heads/feature/edit")), "{refs}");
    git(&home, &work, &["fetch", "-q", "origin", "feature/edit"])?;
    assert_eq!(git(&home, &work, &["show", "-s", "--format=%an|%s", "FETCH_HEAD"])?.trim(), "dev|Say it again");
    assert_eq!(git(&home, &work, &["show", "FETCH_HEAD:.github/workflows/ci.yml"])?, changed);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, runs) = http(s.port, "GET", "/api/runs?limit=50", &s.admin, None)?;
        if runs.contains(&sha) {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "no run for the commit: {runs}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
    let (_, audit) = http(s.port, "GET", "/api/audit?limit=200", &s.admin, None)?;
    assert!(audit.contains("repo.push") && audit.contains("refs/heads/feature/edit"), "{audit}");
    // Again, the same: nothing to commit.
    let (st, v) = post(&s, &dev, edit, &json!({ "content": changed, "to": "branch", "branch": "feature/edit" }))?;
    assert!(st == 409 && v.to_string().contains("already so"), "{st} {v}");
    // By default, a repository kiln hosts takes a diff, landing through review.
    let (st, v) = post(&s, &dev, edit, &json!({ "content": changed }))?;
    assert!(st == 200 && v.get("to") == Some(&json!("diff")) && v.pointer("/diffs/0/name").and_then(Value::as_str).is_some_and(|n| n.starts_with('D')), "{st} {v}");
    // Its changes are diffs, not another host's changes.
    let (st, _) = post(&s, &dev, edit, &json!({ "content": changed, "to": "change" }))?;
    assert_eq!(st, 409);
    // The operation is the route.
    let (st, v) = post(&s, &dev, "/api/ops/ci.workflows.edit", &json!({ "repo": "acme/app", "path": ".github/workflows/ci.yml", "content": changed, "to": "patch" }))?;
    assert!(st == 200 && v.get("patch").and_then(Value::as_str).is_some_and(|p| p.contains("+      - run: echo built, again")), "{st} {v}");
    let (st, _) = post(&s, &viewer, "/api/ops/ci.workflows.edit", &json!({ "repo": "acme/app", "path": ".github/workflows/ci.yml", "content": changed, "to": "patch" }))?;
    assert_eq!(st, 403);
    Ok(())
}

const GREET: &str = r#"name: Greet
description: Says hello.
inputs:
  who:
    description: Who to greet.
    default: world
outputs:
  greeting:
    description: What it said.
    value: ${{ steps.say.outputs.greeting }}
runs:
  using: composite
  steps:
    - id: say
      shell: bash
      env:
        WHO: ${{ inputs.who }}
      run: echo "greeting=hello $WHO" >> "$GITHUB_OUTPUT"
"#;

/// W21-R07: an action in its editor, listed with the workflows and opened as they are, and tried
/// as it is there, saved or not: a run of it with the inputs given, its outputs the job's.
#[test]
fn w21_r07_an_action_tried() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    git(&home, dir.path(), &["clone", "-q", &format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port), "work"])?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join(".github/actions/greet"))?;
    std::fs::write(work.join(".github/actions/greet/action.yml"), GREET)?;
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "an action"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;
    let path = ".github/actions/greet/action.yml";
    // Listed beside the workflows, and opened as one is.
    let deadline = Instant::now() + Duration::from_secs(60);
    let listed = loop {
        let (st, body) = http(s.port, "GET", "/api/ci/repos/acme%2Fapp/workflows", &dev, None)?;
        if st == 200 && body.contains(path) {
            break serde_json::from_str::<Value>(&body)?;
        }
        anyhow::ensure!(Instant::now() < deadline, "the action isn't listed: {st} {body}");
        std::thread::sleep(Duration::from_millis(300));
    };
    assert_eq!(listed.get("actions"), Some(&json!([path])), "{listed}");
    let (st, body) = http(s.port, "GET", &format!("/api/ci/repos/acme%2Fapp/workflows/{path}"), &dev, None)?;
    let opened: Value = serde_json::from_str(&body)?;
    assert!(st == 200 && opened.get("kind") == Some(&json!("action")) && opened.get("source") == Some(&json!(GREET)), "{st} {body}");
    // Tried as edited, not as committed: the job's output is the edited action's.
    let edited = GREET.replace("hello", "hi");
    let try_it = "/api/ci/repos/acme%2Fapp/actions/try";
    let (st, v) = post(&s, &s.admin, try_it, &json!({ "path": path, "content": edited, "inputs": { "who": "ada" }, "runs_on": "local" }))?;
    assert!(st == 201 && v.get("trusted") == Some(&json!(true)), "{st} {v}");
    let run = v.get("run").and_then(Value::as_str).unwrap_or_default().to_owned();
    let deadline = Instant::now() + Duration::from_secs(120);
    let detail = loop {
        let (_, body) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
        let d: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        if d.pointer("/run/finished_at").is_some_and(|f| !f.is_null()) {
            break d;
        }
        anyhow::ensure!(Instant::now() < deadline, "the run didn't finish: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let leg = detail.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (_, body) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
    let leg_detail: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let mut log = String::new();
    for step in 0..4 {
        let (_, l) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
        log.push_str(&format!("-- step {step}\n{l}\n"));
    }
    log.push_str(&format!("-- leg\n{leg_detail}\n"));
    assert_eq!(detail.pointer("/run/conclusion"), Some(&json!("success")), "{detail}\n{log}\n{}", s.log());
    assert_eq!(leg_detail.pointer("/outputs/greeting"), Some(&json!("hi ada")), "{leg_detail}\n{log}");
    // Anyone else's try is a fork's.
    let (st, v) = post(&s, &dev, try_it, &json!({ "path": path, "content": GREET, "runs_on": "local" }))?;
    assert!(st == 201 && v.get("trusted") == Some(&json!(false)), "{st} {v}");
    // What it can't try.
    let (st, v) = post(&s, &dev, try_it, &json!({ "path": path, "content": GREET, "inputs": { "whom": "x" }, "runs_on": "local" }))?;
    assert!(st == 400 && v.to_string().contains("no input `whom`"), "{st} {v}");
    let (st, v) = post(&s, &dev, try_it, &json!({ "path": path, "content": "name: x\nruns: {}\n", "runs_on": "local" }))?;
    assert!(st == 422 && v.to_string().contains("has errors"), "{st} {v}");
    let (st, _) = post(&s, &dev, try_it, &json!({ "path": ".github/workflows/ci.yml", "content": GREET, "runs_on": "local" }))?;
    assert_eq!(st, 400);
    let (st, _) = post(&s, &dev, try_it, &json!({ "path": path, "content": GREET, "runs_on": "local; rm" }))?;
    assert_eq!(st, 400);
    Ok(())
}
