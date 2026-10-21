//! W14-AC05 and W07-R09 (docs/ci.md): GitHub's own actions against a native repository, end to
//! end: `actions/checkout` at a diff's version with its submodule, `actions/download-artifact`
//! across runs, and a comment action on the pull request. They come from github.com (pinned as the
//! run starts and mirrored), so the test runs only when github.com answers.

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

/// Whether github.com answers here.
fn github() -> bool {
    Command::new("curl").args(["-fsSI", "--max-time", "10", "https://github.com/actions/checkout"]).output().is_ok_and(|o| o.status.success())
}

/// The newest finished run for `event`: it and its first leg's steps' logs.
fn finished(s: &Server, event: &str, secs: u64) -> anyhow::Result<(Value, String)> {
    let mut run = Value::Null;
    until(s, secs, &format!("a {event} run"), || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=10", &s.admin, None)?;
        let runs = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        match runs.into_iter().find(|r| r.get("event").and_then(Value::as_str) == Some(event)) {
            Some(r) if r.get("finished_at").is_some_and(|f| !f.is_null()) => {
                run = r;
                Ok(None)
            }
            _ => Ok(Some(body)),
        }
    })?;
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
    let mut log = String::new();
    for step in serde_json::from_str::<Value>(&about)?.get("steps").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.get("idx").and_then(Value::as_u64)) {
        let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
        log.push_str(&text);
    }
    Ok((run, log))
}

const PRODUCE: &str = r#"on:
  push: { branches: [main] }
jobs:
  produce:
    runs-on: local
    steps:
      - run: echo "made in run one" > made.txt
      - uses: actions/upload-artifact@v4
        with: { name: made, path: made.txt }
"#;

const CONSUME: &str = r#"on: pull_request
permissions:
  contents: read
  actions: read
  pull-requests: write
  issues: write
jobs:
  consume:
    runs-on: local
    steps:
      - uses: actions/checkout@v4
        with:
          ref: ${{ github.event.pull_request.head.sha }}
          submodules: true
      - run: |
          test "$(cat change.txt)" = changed && echo "at the diff's version"
          test "$(cat sub/lib.txt)" = "the library" && echo "with its submodule"
      - uses: actions/download-artifact@v4
        with:
          name: made
          run-id: ${{ vars.PRODUCER_RUN }}
          github-token: ${{ github.token }}
      - run: test "$(cat made.txt)" = "made in run one" && echo "from the other run"
      - uses: peter-evans/create-or-update-comment@v4
        with:
          issue-number: ${{ github.event.pull_request.number }}
          body: from an action
"#;

/// W14-AC05's parts left (with W07-R09): a pull request's job checks the diff's version out with
/// its submodule, downloads an artifact another run made, and comments on the pull request, all
/// with GitHub's own actions against kiln.
#[test]
fn w14_ac05_github_actions_on_a_native_repository() -> anyhow::Result<()> {
    if !github() {
        eprintln!("skipped: github.com doesn't answer here, and the actions come from it");
        return Ok(());
    }
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let s = start(dir.path(), &Options { agent: true, ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&home, dir.path(), &["clone", "-q", &url, "work"])?;
    let work = dir.path().join("work");
    // The trunk first, so it's the trunk.
    git(&home, &work, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "README.md"])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    // The library the submodule names: a branch of the repository itself.
    git(&home, &work, &["checkout", "-q", "--orphan", "lib"])?;
    git(&home, &work, &["rm", "-rfq", "--cached", "."])?;
    std::fs::remove_file(work.join("README.md"))?;
    std::fs::write(work.join("lib.txt"), "the library")?;
    git(&home, &work, &["add", "lib.txt"])?;
    git(&home, &work, &["commit", "-qm", "the library"])?;
    git(&home, &work, &["push", "-q", "origin", "lib"])?;
    git(&home, &work, &["checkout", "-q", "main"])?;
    git(&home, &work, &["-c", "protocol.file.allow=always", "submodule", "add", "-q", "-b", "lib", "../app.git", "sub"])?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(work.join(".github/workflows/produce.yml"), PRODUCE)?;
    std::fs::write(work.join(".github/workflows/consume.yml"), CONSUME)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "the workflows and the library"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;
    // The producer's run, and its number for the consumer.
    let (produced, log) = finished(&s, "push", 600)?;
    assert_eq!(produced.get("conclusion"), Some(&json!("success")), "{produced}\n{log}\n{}", s.log());
    let number = produced.get("github_run_id").cloned().unwrap_or_default();
    let (st, body) = http(s.port, "PUT", "/api/ci/variables/PRODUCER_RUN?scope=repo:acme/app", &s.admin, Some(&json!({ "value": number.to_string() })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    // A diff, and its pull request's run.
    std::fs::write(work.join("change.txt"), "changed")?;
    git(&home, &work, &["add", "change.txt"])?;
    git(&home, &work, &["commit", "-qm", "a change\n\nChange-Id: I2222222222222222222222222222222222222222"])?;
    git(&home, &work, &["push", "-q", "origin", "HEAD:refs/for/main"])?;
    let (consumed, log) = finished(&s, "pull_request", 900)?;
    assert_eq!(consumed.get("conclusion"), Some(&json!("success")), "{consumed}\n{log}\n{}", s.log());
    for said in ["at the diff's version", "with its submodule", "from the other run"] {
        assert!(log.contains(said), "{said}: {log}");
    }
    // The comment is the diff's, as the job's.
    let (_, threads) = http(s.port, "GET", "/api/diffs/D1/comments", &s.admin, None)?;
    assert!(threads.contains("from an action"), "{threads}");
    Ok(())
}
