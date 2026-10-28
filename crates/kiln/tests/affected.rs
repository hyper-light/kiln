//! W24-R17 (docs/ci.md): `affected()` in real runs, end to end. The run's changed files are mapped
//! to the packages of the commit's package graph, read from the server's mirror, and a matrix over
//! `affected('cargo')` runs only the legs of the crates a push can affect.

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

/// A repository of the server's own, its trunk pushed once (a README), and a way to commit and
/// push what's in its working tree.
struct Repo {
    home: std::path::PathBuf,
    work: std::path::PathBuf,
    push: String,
}

impl Repo {
    fn new(s: &Server, dir: &Path, name: &str) -> anyhow::Result<Repo> {
        let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": name, "mode": "native" })))?;
        anyhow::ensure!(st == 200, "{st} {body}");
        let r = Repo { home: dir.join("githome"), work: dir.join("tree"), push: format!("http://x:{}@127.0.0.1:{}/git/{name}.git", s.admin, s.port) };
        std::fs::create_dir_all(&r.home)?;
        std::fs::create_dir_all(&r.work)?;
        git(&r.home, &r.work, &["init", "-q", "-b", "main"])?;
        r.write("README.md", "a workspace\n")?;
        r.commit("first")?;
        until(s, 60, "the first poll", || {
            let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
            Ok(body.contains("\"last_polled_at\":null").then_some(body))
        })?;
        Ok(r)
    }

    fn write(&self, path: &str, text: &str) -> anyhow::Result<()> {
        let at = self.work.join(path);
        if let Some(d) = at.parent() {
            std::fs::create_dir_all(d)?;
        }
        Ok(std::fs::write(at, text)?)
    }

    fn commit(&self, message: &str) -> anyhow::Result<()> {
        git(&self.home, &self.work, &["add", "."])?;
        git(&self.home, &self.work, &["commit", "-qm", message])?;
        git(&self.home, &self.work, &["push", "-q", &self.push, "HEAD:main"])?;
        Ok(())
    }

    /// Two crates, `b` depending on `a`, and `.kiln/` holding `workflows`.
    fn workspace(&self, workflows: &[(&str, &str)]) -> anyhow::Result<()> {
        self.write("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n")?;
        self.write("crates/a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\n")?;
        self.write("crates/a/src/lib.rs", "pub fn a() {}\n")?;
        self.write("crates/b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\n\n[dependencies]\na = { path = \"../a\" }\n")?;
        self.write("crates/b/src/lib.rs", "pub fn b() {}\n")?;
        for (name, text) in workflows {
            self.write(&format!(".kiln/{name}"), text)?;
        }
        Ok(())
    }
}

/// The `count`th run of `workflow` once it has finished: its detail, and its legs (`key` →
/// detail) as the API gives them.
fn finished(s: &Server, repo: &str, workflow: &str, count: usize) -> anyhow::Result<(Value, std::collections::BTreeMap<String, Value>)> {
    let mut run = Value::Null;
    until(s, 300, &format!("run {count} of {workflow} to finish"), || {
        let (_, body) = http(s.port, "GET", &format!("/api/ci/runs?repo={repo}&limit=50"), &s.admin, None)?;
        let mut runs: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        runs.retain(|r| r.get("workflow").and_then(Value::as_str) == Some(workflow));
        runs.reverse();
        match runs.get(count - 1) {
            Some(r) if r.get("finished_at").is_some_and(|f| !f.is_null()) => {
                run = r.clone();
                Ok(None)
            }
            _ => Ok(Some(body)),
        }
    })?;
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let detail: Value = serde_json::from_str(&detail)?;
    let mut legs = std::collections::BTreeMap::new();
    for l in detail.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
        let leg = l.get("id").and_then(Value::as_str).unwrap_or_default();
        let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
        let about: Value = serde_json::from_str(&about)?;
        legs.insert(about.pointer("/leg/key").and_then(Value::as_str).unwrap_or_default().to_owned(), about);
    }
    Ok((detail, legs))
}

const MATRIX: &str = r#"on: push
jobs:
  test:
    runs-on: local
    strategy:
      matrix:
        crate: ${{ affected('cargo') }}
    steps:
      - run: echo "testing ${{ matrix.crate }}"
"#;

/// W24-R17: a matrix over `affected('cargo')` on a two-crate workspace (`b` depends on `a`). The
/// push that brings the workspace changes its `Cargo.toml`, which affects both crates; a change to
/// `b` alone runs only `b`'s leg; a change to `a` runs both, as `b` depends on it.
#[test]
fn w24_r17_a_matrix_over_affected_runs_only_the_changed_crates_legs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: "" })?;
    let repo = Repo::new(&s, dir.path(), "acme/ws")?;
    let keys = |legs: &std::collections::BTreeMap<String, Value>| legs.keys().cloned().collect::<Vec<_>>();
    repo.workspace(&[("matrix.yaml", MATRIX)])?;
    repo.commit("the workspace")?;
    let (first, legs) = finished(&s, "acme/ws", ".kiln/matrix.yaml", 1)?;
    assert_eq!(keys(&legs), ["test (a)", "test (b)"], "the workspace's manifest changed: every crate\n{first}\n{}", s.log());
    assert!(legs.values().all(|l| l.pointer("/leg/conclusion") == Some(&json!("success"))), "{legs:?}");
    repo.write("crates/b/src/lib.rs", "pub fn b() -> u8 { 2 }\n")?;
    repo.commit("b changes")?;
    let (second, legs) = finished(&s, "acme/ws", ".kiln/matrix.yaml", 2)?;
    assert_eq!(keys(&legs), ["test (b)"], "only b's leg\n{second}");
    assert_eq!(legs.get("test (b)").and_then(|l| l.pointer("/leg/conclusion")), Some(&json!("success")), "{legs:?}");
    repo.write("crates/a/src/lib.rs", "pub fn a() -> u8 { 1 }\n")?;
    repo.commit("a changes")?;
    let (third, legs) = finished(&s, "acme/ws", ".kiln/matrix.yaml", 3)?;
    assert_eq!(keys(&legs), ["test (a)", "test (b)"], "a, and b, which depends on it\n{third}");
    Ok(())
}

const CHECKS: &str = r#"on: push
jobs:
  a:
    runs-on: local
    if: contains(affected('cargo'), 'a')
    work: { memo: { paths: [crates/a, docs/a.md] } }
    steps:
      - run: echo "checked a as $(date +%s%N)"
  b:
    runs-on: local
    if: contains(affected('cargo'), 'b')
    work: { memo: { paths: [crates/b] } }
    steps:
      - run: echo "checked b as $(date +%s%N)"
"#;

/// W24-R06 with R17: jobs that opt in to reuse, each run only when the change affects its crate.
/// One the change can't affect finishes reused, naming the run it came from, never skipped, so its
/// required check stays green; with no earlier result of its inputs (a file it lists changed,
/// though no crate of it did), it's skipped, saying why.
#[test]
fn w24_r06_a_leg_the_change_cannot_affect_finishes_reused() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: "" })?;
    let repo = Repo::new(&s, dir.path(), "acme/checks")?;
    repo.workspace(&[("checks.yaml", CHECKS)])?;
    repo.write("docs/a.md", "a, first\n")?;
    repo.commit("the workspace")?;
    let (first, legs) = finished(&s, "acme/checks", ".kiln/checks.yaml", 1)?;
    for key in ["a", "b"] {
        let l = legs.get(key).cloned().unwrap_or_default();
        assert_eq!(l.pointer("/leg/conclusion"), Some(&json!("success")), "{key} runs: {l}\n{first}\n{}", s.log());
        assert!(l.pointer("/leg/reused_from").is_none_or(Value::is_null), "{key} had nothing to reuse: {l}");
    }
    // b changes: a can't be affected, and takes the first run's result.
    repo.write("crates/b/src/lib.rs", "pub fn b() -> u8 { 2 }\n")?;
    repo.commit("b changes")?;
    let (second, legs) = finished(&s, "acme/checks", ".kiln/checks.yaml", 2)?;
    let (a, b) = (legs.get("a").cloned().unwrap_or_default(), legs.get("b").cloned().unwrap_or_default());
    assert_eq!(a.pointer("/leg/conclusion"), Some(&json!("success")), "reused, never skipped: {a}\n{}", s.log());
    assert!(a.pointer("/leg/reused_from").is_some_and(|r| !r.is_null()), "{a}");
    assert_eq!(a.pointer("/leg/reused_run"), first.pointer("/run/id"), "it names the run it came from: {a}\n{first}");
    assert!(b.pointer("/leg/reused_from").is_none_or(Value::is_null) && b.pointer("/leg/conclusion") == Some(&json!("success")), "b ran: {b}");
    assert_eq!(second.pointer("/run/conclusion"), Some(&json!("success")), "{second}");
    // b again, and a file a lists: still no crate of a's, but nothing of its inputs to reuse.
    repo.write("crates/b/src/lib.rs", "pub fn b() -> u8 { 3 }\n")?;
    repo.write("docs/a.md", "a, second\n")?;
    repo.commit("b and a's notes change")?;
    let (third, legs) = finished(&s, "acme/checks", ".kiln/checks.yaml", 3)?;
    let a = legs.get("a").cloned().unwrap_or_default();
    assert_eq!(a.pointer("/leg/status"), Some(&json!("skipped")), "{a}\n{third}");
    let said = a.pointer("/leg/error").and_then(Value::as_str).unwrap_or_default();
    assert!(said.contains("the change doesn't affect it, and no earlier result of its inputs is there to reuse"), "{said}");
    Ok(())
}
