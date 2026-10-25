//! W24-R07 (docs/ci.md): step reuse end to end, on a server with its own runner. A job's memo
//! steps run on a tree once: on the same tree again neither runs, each saying it reused its result
//! and naming the run it came from; a changed input reruns only the steps that depend on it (the
//! files a reused step kept put in place for the one that runs); a failure is never reused.

mod common;

use std::collections::BTreeMap;
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

/// `build` makes a file of `src` and tests it with `docs`; `check` passes once `docs/ok` is there.
const WORKFLOW: &str = r#"on: push
jobs:
  build:
    runs-on: local
    steps:
      - name: build
        run: |
          mkdir -p out
          echo "built $(cat src/version) as $(date +%s%N)" | tee out/bin
        memo: { inputs: [src], outputs: [out] }
      - name: test
        run: echo "tested $(cat out/bin) with $(cat docs/note) as $(date +%s%N)"
        memo: { inputs: [docs] }
  check:
    runs-on: local
    steps:
      - run: echo "checked as $(date +%s%N)"; test -f docs/ok
        memo: check
"#;

/// A step of a finished run: its conclusion, its log, and the notices said of it.
#[derive(Debug, Default, Clone)]
struct StepSeen {
    conclusion: String,
    log: String,
    notices: Vec<String>,
}

impl StepSeen {
    /// The line it printed that starts so.
    fn line(&self, start: &str) -> String {
        self.log.lines().map(str::trim).find(|l| l.starts_with(start)).map(str::to_owned).unwrap_or_default()
    }

    fn reused_from(&self, run: &str) -> bool {
        self.log.contains("Reused from run #") && self.log.contains(&format!("({run})")) && self.notices.iter().any(|n| n.contains(&format!("({run})")))
    }
}

/// The newest run once `count` runs exist and it has finished: its id, and its steps by
/// `<job key>/<step index>`.
fn finished(s: &Server, count: usize) -> anyhow::Result<(String, BTreeMap<String, StepSeen>)> {
    let mut run = String::new();
    until(s, 240, &format!("run {count} to finish"), || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/memo&limit=20", &s.admin, None)?;
        let runs = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        let Some(newest) = runs.first().filter(|_| runs.len() >= count) else { return Ok(Some(body)) };
        if newest.get("finished_at").is_none_or(Value::is_null) {
            return Ok(Some(body));
        }
        run = newest.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok(None)
    })?;
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
    let mut steps = BTreeMap::new();
    for l in serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
        let leg = l.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
        let about: Value = serde_json::from_str(&about)?;
        let key = about.pointer("/leg/key").and_then(Value::as_str).unwrap_or_default().to_owned();
        let notes = about.get("annotations").and_then(Value::as_array).cloned().unwrap_or_default();
        for step in about.get("steps").and_then(Value::as_array).into_iter().flatten() {
            let idx = step.get("idx").and_then(Value::as_u64).unwrap_or_default();
            let (_, log) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={idx}"), &s.admin, None)?;
            let notices = notes.iter().filter(|n| n.get("step").and_then(Value::as_u64) == Some(idx) && n.get("level").and_then(Value::as_str) == Some("notice")).filter_map(|n| n.get("message").and_then(Value::as_str).map(str::to_owned)).collect();
            let conclusion = step.get("conclusion").and_then(Value::as_str).unwrap_or_default().to_owned();
            steps.insert(format!("{key}/{idx}"), StepSeen { conclusion, log, notices });
        }
    }
    Ok((run, steps))
}

/// W24-R07: a two-step job run twice on an unchanged tree runs neither memo step the second time,
/// and each says it reused its result, naming the source run; a changed input reruns only what
/// depends on it; a failed memo step is never reused; `memo: check` is reused once it succeeds.
#[test]
fn w24_r07_steps_are_reused_on_an_unchanged_tree() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: "" })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/memo", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    for d in ["src", "docs", ".kiln"] {
        std::fs::create_dir_all(work.join(d))?;
    }
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/memo.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "memo\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    let commit = |message: &str| -> anyhow::Result<()> {
        git(&home, &work, &["add", "."])?;
        git(&home, &work, &["commit", "-q", "--allow-empty", "-m", message])?;
        git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
        Ok(())
    };
    let step = |steps: &BTreeMap<String, StepSeen>, at: &str| steps.get(at).cloned().unwrap_or_default();

    // The first run: every step runs; `check` fails, and isn't kept.
    std::fs::write(work.join(".kiln/ci.yaml"), WORKFLOW)?;
    std::fs::write(work.join("src/version"), "1\n")?;
    std::fs::write(work.join("docs/note"), "first\n")?;
    commit("the workflow")?;
    let (first, one) = finished(&s, 1)?;
    let (build, test, check) = (step(&one, "build/0"), step(&one, "build/1"), step(&one, "check/0"));
    let built = build.line("built 1 as ");
    anyhow::ensure!(build.conclusion == "success" && !built.is_empty() && !build.log.contains("Reused"), "{one:#?}\n{}", s.log());
    let tested = test.line("tested ");
    anyhow::ensure!(test.conclusion == "success" && tested.starts_with(&format!("tested {built} with first as ")), "{one:#?}");
    anyhow::ensure!(check.conclusion == "failure" && check.log.contains("not kept for reuse"), "{one:#?}");

    // The same tree again: neither memo step runs, each reused from the first run, its log that
    // run's; the failed check runs again.
    commit("again")?;
    let (_, two) = finished(&s, 2)?;
    let (build, test, check2) = (step(&two, "build/0"), step(&two, "build/1"), step(&two, "check/0"));
    anyhow::ensure!(build.conclusion == "success" && build.reused_from(&first) && build.line("built ") == built, "{two:#?}");
    anyhow::ensure!(test.conclusion == "success" && test.reused_from(&first) && test.line("tested ") == tested, "{two:#?}");
    anyhow::ensure!(check2.conclusion == "failure" && !check2.log.contains("Reused") && !check2.line("checked as ").is_empty() && check2.line("checked as ") != check.line("checked as "), "a failure is never reused: {two:#?}");

    // A changed input of `test` alone: `build` is reused, `test` runs on the files the first run's
    // build kept, put in place for it.
    std::fs::write(work.join("docs/note"), "second\n")?;
    commit("docs")?;
    let (_, three) = finished(&s, 3)?;
    let (build, test) = (step(&three, "build/0"), step(&three, "build/1"));
    anyhow::ensure!(build.reused_from(&first) && build.line("built ") == built, "{three:#?}");
    anyhow::ensure!(test.conclusion == "success" && !test.log.contains("Reused") && test.log.contains("put in place first"), "{three:#?}");
    anyhow::ensure!(test.line("tested ").starts_with(&format!("tested {built} with second as ")), "{three:#?}");

    // A changed input of `build`: it runs, and `test`, which takes what it made, runs too; `check`
    // passes now.
    std::fs::write(work.join("src/version"), "2\n")?;
    std::fs::write(work.join("docs/ok"), "ok\n")?;
    commit("version 2")?;
    let (fourth, four) = finished(&s, 4)?;
    let (build, test, check) = (step(&four, "build/0"), step(&four, "build/1"), step(&four, "check/0"));
    let built2 = build.line("built 2 as ");
    anyhow::ensure!(!built2.is_empty() && !build.log.contains("Reused"), "{four:#?}");
    let tested2 = test.line("tested ");
    anyhow::ensure!(tested2.starts_with(&format!("tested {built2} with second as ")) && !test.log.contains("Reused"), "{four:#?}");
    anyhow::ensure!(check.conclusion == "success" && check.log.contains("memo: kept for reuse"), "{four:#?}");

    // That tree again: all three reused from the fourth run.
    commit("once more")?;
    let (_, five) = finished(&s, 5)?;
    let (build, test, check5) = (step(&five, "build/0"), step(&five, "build/1"), step(&five, "check/0"));
    anyhow::ensure!(build.reused_from(&fourth) && build.line("built ") == built2, "{five:#?}");
    anyhow::ensure!(test.reused_from(&fourth) && test.line("tested ") == tested2, "{five:#?}");
    anyhow::ensure!(check5.conclusion == "success" && check5.reused_from(&fourth) && check5.line("checked as ") == check.line("checked as "), "{five:#?}");
    Ok(())
}
