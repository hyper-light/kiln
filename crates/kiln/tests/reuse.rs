//! W08-AC06 (docs/ci.md): job-result reuse end to end. A leg whose inputs (its definition and the
//! listed paths' contents, among the rest) are a succeeded leg's takes that leg's result instead
//! of running, and names its source; a change to a listed path runs it again, and so does a rerun
//! that asks to run anyway.

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

const BUILD: &str = r#"on: push
jobs:
  build:
    runs-on: local
    reuse: { paths: [src] }
    steps:
      - run: echo "built $(cat src/version) as $(date +%s%N)"
"#;

/// The newest run's, once `count` runs exist and it has finished: its leg's detail and its log.
fn finished(s: &Server, count: usize) -> anyhow::Result<(Value, String)> {
    let mut leg = String::new();
    until(s, 180, &format!("run {count} to finish"), || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/reuse&limit=20", &s.admin, None)?;
        let runs = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        let Some(newest) = runs.first().filter(|_| runs.len() >= count) else { return Ok(Some(body)) };
        if newest.get("finished_at").is_none_or(Value::is_null) {
            return Ok(Some(body));
        }
        let id = newest.get("id").and_then(Value::as_str).unwrap_or_default();
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
        leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok(leg.is_empty().then_some(detail))
    })?;
    leg_now(s, &leg)
}

fn leg_now(s: &Server, leg: &str) -> anyhow::Result<(Value, String)> {
    let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
    let about: Value = serde_json::from_str(&about)?;
    let mut log = String::new();
    for step in about.get("steps").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.get("idx").and_then(Value::as_u64)) {
        let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
        log.push_str(&text);
    }
    Ok((about, log))
}

/// The line the build printed.
fn built(log: &str) -> String {
    log.lines().find(|l| l.contains("built ") && l.contains(" as ")).map(|l| l.trim().to_owned()).unwrap_or_default()
}

/// W08-AC06: a README-only push reuses the build (its source named, its log the source's), a
/// change under its `paths` runs it, and a rerun that runs anyway runs it.
#[test]
fn w08_ac06_a_leg_takes_a_result_for_the_same_inputs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: "" })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/reuse", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join("src"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/reuse.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "reuse\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    let commit = |message: &str| -> anyhow::Result<()> {
        git(&home, &work, &["add", "."])?;
        git(&home, &work, &["commit", "-qm", message])?;
        git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
        Ok(())
    };
    // The first build runs.
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".kiln/build.yaml"), BUILD)?;
    std::fs::write(work.join("src/version"), "1\n")?;
    commit("build")?;
    let (first, first_log) = finished(&s, 1)?;
    assert_eq!(first.pointer("/leg/conclusion"), Some(&json!("success")), "{first}\n{first_log}\n{}", s.log());
    assert!(first.pointer("/leg/reused_from").is_none_or(Value::is_null), "the first run has nothing to reuse: {first}");
    let line = built(&first_log);
    assert!(line.contains("built 1"), "{first_log}");
    // A README-only change: the same inputs, so the build's result, its source named.
    std::fs::write(work.join("README.md"), "reuse, again\n")?;
    commit("readme")?;
    let (second, second_log) = finished(&s, 2)?;
    assert_eq!(second.pointer("/leg/conclusion"), Some(&json!("success")), "{second}\n{second_log}");
    assert!(second.pointer("/leg/reused_from").is_some_and(|r| !r.is_null()), "it reused the build: {second}");
    assert_eq!(second.pointer("/leg/reused_run"), first.get("run"), "it names the run it came from: {second}");
    assert_eq!(built(&second_log), line, "its log is its source's");
    // A change under its `paths`: it runs.
    std::fs::write(work.join("src/version"), "2\n")?;
    commit("version 2")?;
    let (third, third_log) = finished(&s, 3)?;
    assert!(third.pointer("/leg/reused_from").is_none_or(Value::is_null), "a listed path changed: {third}");
    assert!(built(&third_log).contains("built 2"), "{third_log}");
    // The reused run again, run anyway: it runs, and says so.
    let second_run = second.get("run").and_then(Value::as_str).unwrap_or_default().to_owned();
    let leg = second.pointer("/leg/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (st, body) = http(s.port, "POST", &format!("/api/ci/runs/{second_run}/rerun"), &s.admin, Some(&json!({ "mode": "all", "no_reuse": true })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    // A rerun's leg is a new leg of the run (or a new attempt of the old one).
    let mut again = (Value::Null, String::new());
    until(&s, 180, "the rerun", || {
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{second_run}"), &s.admin, None)?;
        let now = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or(&leg).to_owned();
        again = leg_now(&s, &now)?;
        let anew = now != leg || again.0.pointer("/leg/attempt").and_then(Value::as_u64).unwrap_or(1) > 1;
        let ran = anew && again.0.pointer("/leg/finished_at").is_some_and(|f| !f.is_null());
        Ok((!ran).then(|| again.0.to_string()))
    })?;
    let (rerun, rerun_log) = again;
    assert!(rerun.pointer("/leg/reused_from").is_none_or(Value::is_null), "run anyway: {rerun}");
    let rebuilt = built(&rerun_log);
    assert!(rebuilt.contains("built 1") && rebuilt != line, "it built again, at the reused commit: {rerun_log}");
    Ok(())
}

const PULL: &str = r#"on: pull_request
jobs:
  build:
    runs-on: local
    reuse: { paths: [src] }
    steps:
      - run: echo "built $(cat src/version) as $(date +%s%N)"
  docs:
    runs-on: local
    reuse: { paths: [docs] }
    steps:
      - run: echo "documented $(cat docs/note) as $(date +%s%N)"
"#;

/// The newest `pull_request` run made at `since` or after, once it has finished: its legs by key,
/// each its detail and its log.
fn pull_run(s: &Server, since: i64) -> anyhow::Result<std::collections::BTreeMap<String, (Value, String)>> {
    let mut run = String::new();
    until(s, 300, "the pull request's run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/reuse&limit=20", &s.admin, None)?;
        let runs = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        let newest = runs.into_iter().find(|r| r.get("event").and_then(Value::as_str) == Some("pull_request") && r.get("created_at").and_then(Value::as_i64).is_some_and(|c| c >= since));
        match newest {
            Some(r) if r.get("finished_at").is_some_and(|f| !f.is_null()) => {
                run = r.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
                Ok(None)
            }
            _ => Ok(Some(body)),
        }
    })?;
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
    let mut legs = std::collections::BTreeMap::new();
    for l in serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
        let (about, log) = leg_now(s, l.get("id").and_then(Value::as_str).unwrap_or_default())?;
        legs.insert(about.pointer("/leg/key").and_then(Value::as_str).unwrap_or_default().to_owned(), (about, log));
    }
    Ok(legs)
}

/// W08-R10 with W14: a diff that only rebases reuses every leg whose paths it didn't touch. Its
/// first version builds and documents; then the trunk moves on, changing the docs; the diff rebased
/// onto it, its own change the same, takes the build's result and documents again.
#[test]
fn w08_r10_a_rebased_diff_reuses_what_it_did_not_touch() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: "" })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/reuse", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    for d in ["src", "docs", ".kiln"] {
        std::fs::create_dir_all(work.join(d))?;
    }
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/reuse.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "reuse\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::write(work.join(".kiln/pull.yaml"), PULL)?;
    std::fs::write(work.join("src/version"), "1\n")?;
    std::fs::write(work.join("docs/note"), "first\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "the workflow"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    // The diff's first version: both run.
    std::fs::write(work.join("src/version"), "2\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "version 2\n\nChange-Id: I3333333333333333333333333333333333333333"])?;
    let since = now();
    git(&home, &work, &["push", "-q", &push, "HEAD:refs/for/main"])?;
    let first = pull_run(&s, since)?;
    for key in ["build", "docs"] {
        let (about, log) = first.get(key).cloned().unwrap_or_default();
        assert_eq!(about.pointer("/leg/conclusion"), Some(&json!("success")), "{key}: {about}\n{log}\n{}", s.log());
        assert!(about.pointer("/leg/reused_from").is_none_or(Value::is_null), "{key} had nothing to reuse: {about}");
    }
    let built_first = built(&first.get("build").cloned().unwrap_or_default().1);
    assert!(built_first.contains("built 2"), "{built_first}");
    // The trunk moves on: its docs change.
    git(&home, &work, &["branch", "-q", "diff"])?;
    git(&home, &work, &["reset", "-q", "--hard", "HEAD~1"])?;
    std::fs::write(work.join("docs/note"), "second\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "the docs again"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    // The diff rebased onto it, its own change the same: a new version.
    git(&home, &work, &["checkout", "-q", "diff"])?;
    git(&home, &work, &["rebase", "-q", "main"])?;
    let since = now();
    git(&home, &work, &["push", "-q", &push, "HEAD:refs/for/main"])?;
    let second = pull_run(&s, since)?;
    let (build, build_log) = second.get("build").cloned().unwrap_or_default();
    assert_eq!(build.pointer("/leg/conclusion"), Some(&json!("success")), "{build}\n{build_log}");
    assert!(build.pointer("/leg/reused_from").is_some_and(|r| !r.is_null()), "the build's paths are as they were: {build}");
    assert_eq!(built(&build_log), built_first, "its log is the first version's");
    let (docs, docs_log) = second.get("docs").cloned().unwrap_or_default();
    assert_eq!(docs.pointer("/leg/conclusion"), Some(&json!("success")), "{docs}\n{docs_log}");
    assert!(docs.pointer("/leg/reused_from").is_none_or(Value::is_null), "the trunk changed the docs: {docs}");
    assert!(docs_log.contains("documented second"), "{docs_log}");
    Ok(())
}

/// Now, in Unix seconds (a run's `created_at`).
fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
