//! Experiments (docs/ci.md W20-R11, AC06): options measured against each other and together, each
//! variant built from the options' changes and measured by ordinary CI runs on routed runners.

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

fn op(s: &Server, name: &str, input: Value) -> anyhow::Result<(u16, Value)> {
    let (st, body) = http(s.port, "POST", &format!("/api/ops/{name}"), &s.admin, Some(&input))?;
    Ok((st, serde_json::from_str(&body).unwrap_or(Value::String(body))))
}

/// The bench: 100 ms, `a` takes 20 off, `b` 10, both together 10 more (they help each other), `c`
/// halves it; `b` and `c` together break it.
const BENCH: &str = r#"on: push
jobs:
  bench:
    runs-on: local
    steps:
      - name: measure
        run: |
          t=100
          if [ -e a ]; then t=$((t - 20)); fi
          if [ -e b ]; then t=$((t - 10)); fi
          if [ -e a ] && [ -e b ]; then t=$((t - 10)); fi
          if [ -e c ]; then t=$((t / 2)); fi
          echo "{\"name\": \"p99_ms\", \"value\": $t, \"unit\": \"ms\"}" >> "$KILN_METRICS"
          if [ -e b ] && [ -e c ]; then echo "b and c fight"; exit 1; fi
"#;

/// W20-AC06: three options, every combination: eight variants, each measured twice by the bench
/// on the local runner, interleaved. The recommendation is the best that passes (`a+c`); `a+b`'s
/// interval is about 0.6 of the baseline; `a` and `b` together are synergistic; `b+c` fails its
/// required check. Choosing a variant records its commit. An option whose change conflicts with
/// another's makes a variant said to conflict, not measured; and a cancelled experiment stops.
#[test]
fn w20_ac06_three_options_every_combination() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::write(work.join(".kiln/bench.yaml"), BENCH)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "bench"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    // Each option a branch with one commit on the base; `d` makes `a` too, differently.
    for (branch, file, text) in [("opt-a", "a", "a\n"), ("opt-b", "b", "b\n"), ("opt-c", "c", "c\n"), ("opt-d", "a", "not a\n")] {
        git(&home, &work, &["checkout", "-q", "-B", branch, "main"])?;
        std::fs::write(work.join(file), text)?;
        git(&home, &work, &["add", "."])?;
        git(&home, &work, &["commit", "-qm", branch])?;
        git(&home, &work, &["push", "-q", &push, &format!("HEAD:{branch}")])?;
    }
    until(&s, 60, "the options' branches", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=20", &s.admin, None)?;
        Ok((!body.contains("\"branch\":\"opt-d\"")).then_some(body))
    })?;

    let criteria = json!([
        { "name": "bench", "kind": "pipeline", "workflow": ".kiln/bench.yaml", "required": true },
        { "name": "time", "kind": "metric", "metric": "p99_ms", "better": "lower", "threshold": { "better": 0.05 } }
    ]);
    let options = json!([{ "name": "a", "branch": "opt-a" }, { "name": "b", "branch": "opt-b" }, { "name": "c", "branch": "opt-c" }]);
    let (st, started) = op(
        &s,
        "experiments.start",
        json!({ "repo": "acme/app", "question": "Which make it faster?", "options": options, "strategy": "all", "criteria": criteria, "repetitions": 2, "max_parallel": 4 }),
    )?;
    anyhow::ensure!((200..300).contains(&st), "{st} {started}");
    let id = started.pointer("/experiment/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let names: Vec<&str> = started.pointer("/experiment/variants").and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.get("name").and_then(Value::as_str)).collect();
    assert_eq!(names, ["baseline", "a", "b", "c", "a+b", "a+c", "b+c", "a+b+c"]);
    let mut e = Value::Null;
    until(&s, 900, "the experiment", || {
        let (_, v) = op(&s, "experiments.get", json!({ "experiment": id }))?;
        e = v.get("experiment").cloned().unwrap_or_default();
        Ok((!matches!(e.get("state").and_then(Value::as_str), Some("done" | "failed" | "cancelled")))
            .then(|| format!("{} of its runs measured", e.get("runs").and_then(Value::as_array).map_or(0, Vec::len))))
    })?;
    assert_eq!(e.get("state"), Some(&json!("done")), "{e}");
    let runs = e.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
    assert_eq!(runs.len(), 16, "eight variants, twice each: {e}");
    assert!(runs.iter().all(|r| r.get("run").is_some_and(|x| x.is_string()) && r.get("conclusion").is_some_and(|c| c == "success" || c == "failure")), "each a run of its own: {e}");
    let results = e.get("results").cloned().unwrap_or_default();
    assert_eq!(results.get("recommendation"), Some(&json!("a+c")), "{results}");
    let variant = |n: &str| results.get("variants").and_then(Value::as_array).and_then(|v| v.iter().find(|x| x.get("name").and_then(Value::as_str) == Some(n)).cloned()).unwrap_or_default();
    let ab = variant("a+b");
    let mid = ab.pointer("/metrics/0/vs_baseline/mid").and_then(Value::as_f64).unwrap_or_default();
    assert!((mid - 0.6).abs() < 0.001 && ab.pointer("/metrics/0/vs_baseline/low").is_some() && ab.pointer("/metrics/0/vs_baseline/high").is_some(), "{ab}");
    assert_eq!(variant("b+c").get("passes"), Some(&json!(false)), "{results}");
    let synergy = results.get("interactions").and_then(Value::as_array).into_iter().flatten().find(|i| i.get("a") == Some(&json!("a")) && i.get("b") == Some(&json!("b"))).cloned().unwrap_or_default();
    assert_eq!(synergy.get("kind"), Some(&json!("synergistic")), "{results}");

    // Chosen: the variant's commit.
    let (st, decided) = op(&s, "experiments.decide", json!({ "experiment": id, "variant": "a+c" }))?;
    anyhow::ensure!(st == 200, "{st} {decided}");
    let commit = decided.pointer("/experiment/variants").and_then(Value::as_array).and_then(|v| v.iter().find(|x| x.get("name") == Some(&json!("a+c")))).and_then(|v| v.get("commit")).cloned();
    assert!(commit.is_some() && decided.pointer("/experiment/decision/commit").cloned() == commit, "{decided}");

    // Options that fight over a file: their variant conflicts, and isn't measured. Cancelled, it stops.
    let (st, second) = op(
        &s,
        "experiments.start",
        json!({ "repo": "acme/app", "options": [{ "name": "a", "branch": "opt-a" }, { "name": "d", "branch": "opt-d" }], "strategy": "all", "baseline": false, "criteria": [{ "name": "bench", "kind": "pipeline", "workflow": ".kiln/bench.yaml", "required": true }], "repetitions": 3, "max_parallel": 1 }),
    )?;
    anyhow::ensure!((200..300).contains(&st), "{st} {second}");
    let id2 = second.pointer("/experiment/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut two = Value::Null;
    until(&s, 120, "its variants built", || {
        let (_, v) = op(&s, "experiments.get", json!({ "experiment": id2 }))?;
        two = v.get("experiment").cloned().unwrap_or_default();
        Ok((two.get("state") != Some(&json!("measuring"))).then(|| two.to_string()))
    })?;
    let ad = two.get("variants").and_then(Value::as_array).and_then(|v| v.iter().find(|x| x.get("name") == Some(&json!("a+d"))).cloned()).unwrap_or_default();
    assert!(ad.get("state") == Some(&json!("conflict")) && ad.get("why").and_then(Value::as_str).is_some_and(|w| w.contains("d conflicts with a in a")), "{ad}");
    let (st, cancelled) = op(&s, "experiments.cancel", json!({ "experiment": id2 }))?;
    assert!(st == 200 && cancelled.pointer("/experiment/state") == Some(&json!("cancelled")), "{st} {cancelled}");
    until(&s, 120, "the cancelled experiment's runs", || {
        let (_, v) = op(&s, "experiments.get", json!({ "experiment": id2 }))?;
        let n = v.pointer("/experiment/runs").and_then(Value::as_array).map_or(0, Vec::len);
        Ok((v.pointer("/experiment/state") != Some(&json!("cancelled")) || n >= 6).then(|| v.to_string()))
    })?;
    let (_, listed) = op(&s, "experiments.list", json!({ "repo": "acme/app" }))?;
    assert_eq!(listed.get("experiments").and_then(Value::as_array).map(Vec::len), Some(2), "{listed}");
    Ok(())
}
