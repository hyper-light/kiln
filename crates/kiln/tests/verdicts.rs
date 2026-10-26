//! Verdicts per unit and test (docs/ci.md W28-R50 to R55, `ci.verdict`), end to end: a native
//! repository with a two-crate Cargo workspace (q depends on p) whose workflow reports each test
//! with TAP, its file naming its crate. A test already failing at the base makes no suspect; a new
//! failure in p names exactly the commits whose diffs could affect p; a quarantined test's failure
//! makes none.

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
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn op(s: &Server, name: &str, input: Value) -> anyhow::Result<(u16, Value)> {
    let (st, body) = http(s.port, "POST", &format!("/api/ops/{name}"), &s.admin, Some(&input))?;
    Ok((st, serde_json::from_str(&body).unwrap_or(Value::String(body))))
}

/// Waits for run `number` of the repository to end.
fn ended(s: &Server, number: i64) -> anyhow::Result<()> {
    until(s, 240, &format!("run #{number}"), || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=10", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body)?;
        let done = v.get("runs").and_then(Value::as_array).is_some_and(|r| r.iter().any(|r| r.get("number").and_then(Value::as_i64) == Some(number) && r.get("conclusion").is_some_and(|c| !c.is_null())));
        Ok((!done).then_some(body))
    })
}

/// A commit's verdict on its one lane.
fn verdict(s: &Server, sha: &str) -> anyhow::Result<Value> {
    let (st, v) = op(s, "ci.verdict", json!({ "repo": "acme/app", "sha": sha }))?;
    anyhow::ensure!(st == 200, "ci.verdict {sha}: {st} {v}");
    v.pointer("/lanes/0/verdict").cloned().ok_or_else(|| anyhow::anyhow!("no lane holds {sha}: {v}"))
}

fn kind(v: &Value) -> &str {
    v.get("kind").and_then(Value::as_str).unwrap_or_default()
}

/// The verdict of one test at the commit.
fn test<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
    v.get("checks").and_then(Value::as_array)?.iter().find(|c| c.pointer("/check/test").and_then(Value::as_str) == Some(name))
}

fn status<'a>(v: &'a Value, name: &str) -> &'a str {
    test(v, name).and_then(|c| c.get("status")).and_then(Value::as_str).unwrap_or_default()
}

/// Each test passes unless its file is there: `base` while base-bug is, `p_works` while
/// crates/p/bug is, `q_works` while crates/q/bug is. The report alone says how they went.
const CI: &str = r#"on: push
jobs:
  test:
    runs-on: local
    quarantine: enforce
    steps:
      - name: tests
        test-results: { path: report.tap, format: tap }
        run: |
          t() { if [ -e "$2" ]; then echo "not ok - $1"; else echo "ok - $1"; fi; printf '  ---\n  file: %s\n  ...\n' "$3"; }
          { echo "TAP version 13"; t base base-bug crates/p/src/lib.rs; t p_works crates/p/bug crates/p/src/lib.rs; t q_works crates/q/bug crates/q/src/lib.rs; } > report.tap
"#;

/// W28-R51, R52, R55: the base fails `base`; of c1..c5, pushed together, c5's run fails only
/// `base`: every commit is covered or direct, none suspect. Then p breaks at d3: d5's run newly
/// fails `p_works`, and d1, d3 and d5, which change p, are its suspects; d2 and d4, which change
/// only q, aren't. Then `q_works`, quarantined, fails: no commit is suspect for it.
#[test]
fn w28_r55_verdicts_per_unit_and_test() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    for d in [".kiln", "crates/p/src", "crates/q/src"] {
        std::fs::create_dir_all(work.join(d))?;
    }
    let write = |file: &str, text: &str| std::fs::write(work.join(file), text);
    let commit = |files: &[(&str, &str)], message: &str| -> anyhow::Result<String> {
        for (f, t) in files {
            write(f, t)?;
        }
        git(&home, &work, &["add", "."])?;
        git(&home, &work, &["commit", "-qm", message])?;
        Ok(git(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned())
    };
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    commit(&[("README.md", "app\n")], "first")?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    let base = commit(
        &[
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
            ("crates/p/Cargo.toml", "[package]\nname = \"p\"\nversion = \"0.1.0\"\n"),
            ("crates/p/src/lib.rs", "pub fn p() {}\n"),
            ("crates/q/Cargo.toml", "[package]\nname = \"q\"\nversion = \"0.1.0\"\n[dependencies]\np = { path = \"../p\" }\n"),
            ("crates/q/src/lib.rs", "pub fn q() {}\n"),
            ("base-bug", "already broken\n"),
            (".kiln/ci.yaml", CI),
        ],
        "the workspace",
    )?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    ended(&s, 1)?;

    // 1. `base` already failed at the base: c1..c5 are covered or direct, none suspect.
    let c: Vec<String> = [("crates/p/src/a.rs", "c1"), ("crates/q/src/a.rs", "c2"), ("crates/p/src/b.rs", "c3"), ("crates/q/src/b.rs", "c4"), ("README.md", "c5")]
        .iter()
        .map(|(f, m)| commit(&[(f, &format!("{m}\n"))], m))
        .collect::<anyhow::Result<_>>()?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    ended(&s, 2)?;
    for (i, sha) in c.iter().enumerate() {
        let v = verdict(&s, sha)?;
        let want = if i == 4 { "direct" } else { "covered" };
        assert_eq!(kind(&v), want, "c{}: {v}", i + 1);
        assert!(v.get("checks").and_then(Value::as_array).is_some_and(|cs| cs.iter().all(|x| x.get("status") != Some(&json!("suspect")))), "c{}: {v}", i + 1);
    }
    let c1 = verdict(&s, c.first().map_or("", String::as_str))?;
    let b = test(&c1, "base").ok_or_else(|| anyhow::anyhow!("no `base` at c1: {c1}"))?;
    assert_eq!(b.get("status"), Some(&json!("covered")), "{c1}");
    assert_eq!(b.get("optimistic"), Some(&json!(true)), "{c1}");
    assert_eq!(b.get("outcome"), None, "covered is never passed: {c1}");
    assert!(b.get("why").and_then(Value::as_str).is_some_and(|w| w.contains("as it already did at the base") && w.contains(base.get(..12).unwrap_or_default())), "{c1}");
    // q depends on p: c1's change to p affects q's test too; c2 changes only q, which p's tests
    // don't depend on.
    assert_eq!(status(&c1, "q_works"), "covered", "{c1}");
    let c2 = verdict(&s, c.get(1).map_or("", String::as_str))?;
    assert_eq!(status(&c2, "p_works"), "unaffected", "{c2}");

    // 2. p breaks at d3: d1, d3 and d5 (they change p) are suspects of d5's new failure; d2 and d4
    // (only q) aren't.
    let d: Vec<String> = [("crates/p/src/c.rs", "d1"), ("crates/q/src/c.rs", "d2"), ("crates/p/bug", "d3"), ("crates/q/src/d.rs", "d4"), ("crates/p/src/d.rs", "d5")]
        .iter()
        .map(|(f, m)| commit(&[(f, &format!("{m}\n"))], m))
        .collect::<anyhow::Result<_>>()?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    ended(&s, 3)?;
    let (c5, d5) = (c.get(4).cloned().unwrap_or_default(), d.get(4).cloned().unwrap_or_default());
    for (i, sha) in d.iter().enumerate() {
        let v = verdict(&s, sha)?;
        if i % 2 == 0 {
            assert_eq!(kind(&v), "suspect", "d{}: {v}", i + 1);
            let p = test(&v, "p_works").ok_or_else(|| anyhow::anyhow!("no `p_works` at d{}: {v}", i + 1))?;
            assert!(p.get("status") == Some(&json!("suspect")) && p.get("by") == Some(&json!(d5)) && p.get("base") == Some(&json!(c5)), "d{}: {v}", i + 1);
        } else {
            assert_eq!(kind(&v), "covered", "d{}: {v}", i + 1);
            assert_eq!(status(&v, "p_works"), "unaffected", "d{}: {v}", i + 1);
            assert_eq!(status(&v, "q_works"), "covered", "d{}: {v}", i + 1);
        }
        assert_ne!(status(&v, "base"), "suspect", "d{}: {v}", i + 1);
    }

    // 3. q_works, quarantined, fails from e1: no commit is suspect for it, and its verdict says why.
    let e: Vec<String> = [(".kiln/quarantine.yaml", "- test: q_works\n  reason: being looked at\n"), ("crates/q/bug", "e1\n"), ("crates/q/src/e.rs", "e2\n")]
        .iter()
        .enumerate()
        .map(|(i, (f, t))| commit(&[(f, t)], &format!("e{i}")))
        .collect::<anyhow::Result<_>>()?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    ended(&s, 4)?;
    for (i, sha) in e.iter().enumerate() {
        let v = verdict(&s, sha)?;
        assert_ne!(status(&v, "q_works"), "suspect", "e{i}: {v}");
        assert_ne!(kind(&v), "suspect", "e{i}: {v}");
    }
    let e1 = verdict(&s, e.get(1).map_or("", String::as_str))?;
    assert!(test(&e1, "q_works").and_then(|x| x.get("why")).and_then(Value::as_str).is_some_and(|w| w.contains("quarantined")), "{e1}");
    Ok(())
}
