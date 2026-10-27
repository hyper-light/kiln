//! W28-R72 to R75 (docs/ci.md): identical portable memo steps in two pipelines, reached at once on
//! a server with its own runner, run once. One leg's step executes it under a lease; the other's
//! waits, saying on what, then replays its success from the ledger, naming the run it came from,
//! with byte-identical files put in place. A failure is never handed over: the waiter takes its
//! turn and runs the step itself.

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

/// A workflow `name` whose job `job` builds `out/x` in a portable memo step, slowly, then prints
/// it in a step that isn't a memo step (so the files are put in place for it). `extra` goes into
/// the job (its own variables, say), so the two workflows differ everywhere but the step.
fn workflow(name: &str, job: &str, extra: &str, script: &str) -> String {
    format!(
        "name: {name}\non: push\njobs:\n  {job}:\n    runs-on: local\n{extra}    steps:\n      - name: build\n        run: {script}\n        memo: {{ inputs: [src], outputs: [out], platform: any }}\n      - name: show\n        run: echo \"shown $(cat out/x)\"\n"
    )
}

const BUILD: &str = "sleep 6; mkdir -p out; echo \"made $(cat src/v) at $(date +%s%N)\" > out/x";
const FAIL: &str = "sleep 6; echo \"tried $(cat src/v)\"; exit 1";

#[derive(Debug, Default, Clone)]
struct StepSeen {
    conclusion: String,
    log: String,
}

/// Every run once `count` exist and all have finished: each run's id, workflow and steps by index.
fn finished(s: &Server, count: usize) -> anyhow::Result<Vec<(String, String, BTreeMap<u64, StepSeen>)>> {
    let mut runs = Vec::new();
    until(s, 300, &format!("{count} runs to finish"), || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/share&limit=20", &s.admin, None)?;
        runs = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok((runs.len() < count || runs.iter().take(count).any(|r| r.get("finished_at").is_none_or(Value::is_null))).then_some(body))
    })?;
    let mut out = Vec::new();
    for r in runs.iter().take(count) {
        let run = r.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
        let detail: Value = serde_json::from_str(&detail)?;
        let wf = detail.pointer("/run/workflow").or_else(|| detail.get("workflow")).and_then(Value::as_str).unwrap_or_default().to_owned();
        let mut steps = BTreeMap::new();
        for l in detail.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
            let leg = l.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
            let about: Value = serde_json::from_str(&about)?;
            for step in about.get("steps").and_then(Value::as_array).into_iter().flatten() {
                let idx = step.get("idx").and_then(Value::as_u64).unwrap_or_default();
                let (_, log) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={idx}"), &s.admin, None)?;
                steps.insert(idx, StepSeen { conclusion: step.get("conclusion").and_then(Value::as_str).unwrap_or_default().to_owned(), log });
            }
        }
        out.push((run, wf, steps));
    }
    Ok(out)
}

fn line(log: &str, start: &str) -> String {
    log.lines().map(str::trim).find(|l| l.starts_with(start)).map(str::to_owned).unwrap_or_default()
}

/// W28-AC72, AC75: two pipelines reach the same portable step together: one execution; the other
/// waits on it and replays it, naming its run, and the files it shows are the same bytes.
#[test]
fn w28_ac75_two_pipelines_share_one_execution() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: "" })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/share", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    for d in ["src", ".kiln"] {
        std::fs::create_dir_all(work.join(d))?;
    }
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/share.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "share\n")?;
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

    // Two workflows, different names, jobs and job variables; the same portable step.
    std::fs::write(work.join(".kiln/one.yaml"), workflow("one", "ui", "", BUILD))?;
    std::fs::write(work.join(".kiln/two.yaml"), workflow("two", "release", "    env:\n      CARGO_PROFILE_DEV_DEBUG: \"0\"\n", BUILD))?;
    std::fs::write(work.join("src/v"), "1\n")?;
    commit("two pipelines")?;
    let runs = finished(&s, 2)?;
    let builds: Vec<&StepSeen> = runs.iter().filter_map(|(_, _, steps)| steps.get(&0)).collect();
    anyhow::ensure!(builds.len() == 2 && builds.iter().all(|b| b.conclusion == "success"), "{runs:#?}\n{}", s.log());
    // The waiter's log is the producer's, replayed after its own lines (W24-R07), so the producer
    // is the one that reused nothing.
    let executed: Vec<usize> = (0..2).filter(|i| builds.get(*i).is_some_and(|b| b.log.contains("the same step elsewhere waits on it") && !b.log.contains("Reused from run #"))).collect();
    anyhow::ensure!(executed.len() == 1, "exactly one execution: {runs:#?}");
    let (e, w) = (executed.first().copied().unwrap_or(0), 1 - executed.first().copied().unwrap_or(0));
    let (producer, waiter) = (runs.get(e).map(|r| r.0.clone()).unwrap_or_default(), builds.get(w).map(|b| b.log.clone()).unwrap_or_default());
    anyhow::ensure!(waiter.contains("memo: waiting on step 0 of run #"), "the waiter says what it waits on: {waiter}");
    anyhow::ensure!(waiter.contains("Reused from run #") && waiter.contains(&format!("({producer})")), "provenance names the producing run: {waiter}");
    let shown: Vec<String> = runs.iter().filter_map(|(_, _, steps)| steps.get(&1)).map(|s| line(&s.log, "shown made 1 at ")).collect();
    anyhow::ensure!(shown.len() == 2 && !shown.iter().any(String::is_empty) && shown.first() == shown.get(1), "the same bytes in both: {shown:?}\n{runs:#?}");

    // A push touching nothing the step reads: both pipelines reuse it from the ledger.
    std::fs::write(work.join("README.md"), "share, again\n")?;
    commit("readme")?;
    let again = finished(&s, 4)?;
    for (_, _, steps) in again.iter().take(2) {
        let b = steps.get(&0).cloned().unwrap_or_default();
        anyhow::ensure!(b.conclusion == "success" && b.log.contains("Reused from run #") && !b.log.contains("waiting on"), "{again:#?}");
    }
    Ok(())
}

/// W28-AC73: a failure is never handed over: the waiter takes its turn and runs the step itself.
#[test]
fn w28_ac73_a_failure_is_the_waiters_turn() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: "" })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/share", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    for d in ["src", ".kiln"] {
        std::fs::create_dir_all(work.join(d))?;
    }
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/share.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "share\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::write(work.join(".kiln/one.yaml"), workflow("one", "ui", "", FAIL))?;
    std::fs::write(work.join(".kiln/two.yaml"), workflow("two", "release", "", FAIL))?;
    std::fs::write(work.join("src/v"), "1\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "failing"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let runs = finished(&s, 2)?;
    let builds: Vec<StepSeen> = runs.iter().filter_map(|(_, _, steps)| steps.get(&0).cloned()).collect();
    anyhow::ensure!(builds.len() == 2 && builds.iter().all(|b| b.conclusion == "failure" && b.log.contains("tried 1")), "both ran it: {runs:#?}\n{}", s.log());
    anyhow::ensure!(builds.iter().filter(|b| b.log.contains("memo: waited ")).count() == 1, "one waited for the other's turn: {builds:#?}");
    anyhow::ensure!(!builds.iter().any(|b| b.log.contains("Reused from")), "a failure is never reused: {builds:#?}");
    Ok(())
}
