//! W26-R22 (docs/ci.md): Pause freezes a running job where it runs and Resume lets it go on, over
//! HTTP, on a server with a runner of its own (`local`, which says it can freeze a job: Unix). A
//! `.kiln/` pipeline's job (v1) and a GitHub workflow's leg (the engine's) each count ten seconds,
//! a tick a second, into a file. Paused, each reads paused, its step's processes are frozen (their
//! cgroup's `cgroup.freeze` says 1, or, with no cgroup to write, they're stopped) and no tick
//! comes; resumed, each runs on and finishes. The leg's step may take 15 s: paused 12 s of its run,
//! it passes after more than that all told, its time limit stopped while it was paused. A job
//! waiting for the one it needs, paused, isn't dispatched once that one ends, until it's resumed;
//! one that has ended can't be paused, and says so.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, Server, http, start};
use serde_json::{Value, json};

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(())
}

fn get(s: &Server, path: &str) -> anyhow::Result<Value> {
    let (_, body) = http(s.port, "GET", path, &s.admin, None)?;
    Ok(serde_json::from_str(&body).unwrap_or_default())
}

fn post(s: &Server, path: &str, body: &Value) -> anyhow::Result<(u16, Value)> {
    let (st, body) = http(s.port, "POST", path, &s.admin, Some(body))?;
    Ok((st, serde_json::from_str(&body).unwrap_or_default()))
}

/// Waits until `f` has what's waited for, at most `secs`.
fn until<T>(s: &Server, secs: u64, what: &str, mut f: impl FnMut() -> anyhow::Result<Result<T, String>>) -> anyhow::Result<T> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match f()? {
            Ok(v) => return Ok(v),
            Err(why) => anyhow::ensure!(Instant::now() < deadline, "waited {secs}s for {what}: {why}\n--- the server:\n{}", s.log()),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// A run's jobs (v1's `jobs`, the engine's `legs`) by key.
fn by_key(list: Option<&Vec<Value>>) -> std::collections::HashMap<String, Value> {
    list.into_iter().flatten().map(|j| (text(j, "key").to_owned(), j.clone())).collect()
}

/// The answer's word for job `id`, from `{ jobs: [{ job, <field>, why? }] }`.
fn said(v: &Value, id: &str, field: &str) -> (bool, String) {
    let one = v.get("jobs").and_then(Value::as_array).and_then(|a| a.iter().find(|j| text(j, "job") == id)).cloned().unwrap_or_default();
    (one.get(field).and_then(Value::as_bool).unwrap_or(false), text(&one, "why").to_owned())
}

fn ticks(marks: &Path, name: &str) -> usize {
    std::fs::read_to_string(marks.join(name)).map(|t| t.lines().filter(|l| l.starts_with("tick")).count()).unwrap_or(0)
}

/// How job `id`'s step processes stand: each one's cgroup freezer (`freeze=1`: frozen) where it
/// has a cgroup of its job's, else its state (`state=T`: stopped). Found by the job's id in their
/// environment: v1's `KILN_JOB_ID`, the engine's `KILN_LEG`.
fn frozen_state(id: &str) -> Vec<(u32, String)> {
    let marks = [format!("KILN_JOB_ID={id}"), format!("KILN_LEG={id}")];
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        let Ok(env) = std::fs::read(format!("/proc/{pid}/environ")) else { continue };
        if !env.split(|b| *b == 0).any(|v| marks.iter().any(|m| v == m.as_bytes())) {
            continue;
        }
        let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default();
        let path = cgroup.lines().find_map(|l| l.strip_prefix("0::")).unwrap_or_default().trim().to_owned();
        let state = if path.contains("/kiln-jobs-") {
            format!("freeze={}", std::fs::read_to_string(format!("/sys/fs/cgroup{path}/cgroup.freeze")).unwrap_or_default().trim())
        } else {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            format!("state={}", stat.rsplit_once(')').and_then(|(_, r)| r.split_whitespace().next()).unwrap_or("?"))
        };
        out.push((pid, state));
    }
    out
}

#[test]
fn w26_r22_a_running_job_paused_is_frozen_in_place_and_goes_on_when_resumed() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();
    let repo = dir.join("repo");
    let marks = dir.join("marks");
    std::fs::create_dir_all(&repo)?;
    std::fs::create_dir_all(&marks)?;
    // The first commit runs nothing: the pipelines come with the second, so each runs once.
    std::fs::write(repo.join("README.md"), "pause\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::create_dir_all(repo.join(".github/workflows"))?;
    let count = |name: &str| format!("for i in 1 2 3 4 5 6 7 8 9 10; do echo tick $i >> {}; sleep 1; done", marks.join(name).display());
    std::fs::write(
        repo.join(".kiln/pipeline.yaml"),
        format!(
            "name: v1\non:\n  push:\n    branches: [main]\njobs:\n  first:\n    runs-on: [local]\n    steps: [{{run: '{}'}}]\n  then:\n    needs: [first]\n    runs-on: [local]\n    steps: [{{run: 'echo then >> {}'}}]\n",
            count("v1"),
            marks.join("then").display()
        ),
    )?;
    std::fs::write(repo.join(".github/workflows/v2.yml"), format!("name: v2\non: push\njobs:\n  slow:\n    runs-on: [self-hosted, local]\n    steps:\n      - run: '{}'\n        timeout-minutes: 0.25\n", count("v2")))?;
    let extra = format!("[[repos]]\nname = \"acme/pause\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    let s = start(dir, &Options { agent: true, ci: "github_workflows = \"on\"", extra: &extra })?;
    until(&s, 120, "the repository's first poll", || {
        let (_, repos) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(if repos.contains("acme/pause") && !repos.contains("\"last_polled_at\":null") { Ok(()) } else { Err(repos) })
    })?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "go"])?;

    // Both counting: v1's first and the engine's slow, two ticks in at least.
    let runs = |pipeline: &'static str| {
        let s = &s;
        move || -> anyhow::Result<Option<String>> {
            let runs = get(s, "/api/runs?repo=acme/pause")?;
            let list = runs.as_array().cloned().or_else(|| runs.get("runs").and_then(Value::as_array).cloned()).unwrap_or_default();
            Ok(list.iter().find(|r| text(r, "pipeline") == pipeline).map(|r| text(r, "id").to_owned()))
        }
    };
    let (v1_run, v2_run) = until(&s, 180, "both runs counting", || {
        let (a, b) = (runs("v1")()?, runs("v2")()?);
        Ok(match (a, b) {
            (Some(a), Some(b)) if ticks(&marks, "v1") >= 2 && ticks(&marks, "v2") >= 2 => Ok((a, b)),
            other => Err(format!("{other:?}, ticks {} and {}", ticks(&marks, "v1"), ticks(&marks, "v2"))),
        })
    })?;
    let v1 = |s: &Server| -> anyhow::Result<std::collections::HashMap<String, Value>> { Ok(by_key(get(s, &format!("/api/runs/{v1_run}"))?.get("jobs").and_then(Value::as_array))) };
    let v2 = |s: &Server| -> anyhow::Result<std::collections::HashMap<String, Value>> { Ok(by_key(get(s, &format!("/api/ci/runs/{v2_run}"))?.get("legs").and_then(Value::as_array))) };
    let jobs = v1(&s)?;
    let (first, then) = (jobs.get("first").map(|j| text(j, "id").to_owned()).unwrap_or_default(), jobs.get("then").map(|j| text(j, "id").to_owned()).unwrap_or_default());
    let slow = v2(&s)?.get("slow").map(|j| text(j, "id").to_owned()).unwrap_or_default();
    anyhow::ensure!(!first.is_empty() && !then.is_empty() && !slow.is_empty(), "{jobs:?}");

    // Pause both running ones, and the one waiting for first.
    let (st, v) = post(&s, &format!("/api/runs/{v1_run}/pause"), &json!({ "jobs": [first, then] }))?;
    anyhow::ensure!(st == 200 && said(&v, &first, "paused").0 && said(&v, &then, "paused").0, "v1 pause: {st} {v}");
    let (st, v) = post(&s, &format!("/api/runs/{v2_run}/pause"), &json!({ "jobs": [slow] }))?;
    anyhow::ensure!(st == 200 && said(&v, &slow, "paused").0, "the engine's pause: {st} {v}");
    let paused_at = Instant::now();
    anyhow::ensure!(v1(&s)?.get("first").is_some_and(|j| j.get("paused_at").is_some_and(Value::is_i64) && text(j, "status") == "running"), "first reads paused, running: {:?}", v1(&s)?.get("first"));
    anyhow::ensure!(v2(&s)?.get("slow").is_some_and(|j| j.get("paused_at").is_some_and(Value::is_i64)), "slow reads paused: {:?}", v2(&s)?.get("slow"));
    // Frozen where they run: their processes' cgroups (or the processes themselves) say so, and no
    // tick comes while they are.
    for id in [&first, &slow] {
        let state = frozen_state(id);
        let ps = Command::new("ps").args(["-eo", "pid,stat,args"]).output().map(|o| String::from_utf8_lossy(&o.stdout).lines().filter(|l| l.contains(&*marks.to_string_lossy())).collect::<Vec<_>>().join("\n")).unwrap_or_default();
        anyhow::ensure!(!state.is_empty() && state.iter().all(|(_, s)| s == "freeze=1" || s == "state=T"), "job {id}'s processes frozen: {state:?}\n{ps}");
    }
    let (a, b) = (ticks(&marks, "v1"), ticks(&marks, "v2"));
    std::thread::sleep(Duration::from_secs(4));
    anyhow::ensure!((ticks(&marks, "v1"), ticks(&marks, "v2")) == (a, b), "no tick while paused: {a} and {b} became {} and {}", ticks(&marks, "v1"), ticks(&marks, "v2"));
    // Paused again: nothing changes.
    let (_, v) = post(&s, &format!("/api/runs/{v2_run}/pause"), &json!({ "jobs": [slow] }))?;
    anyhow::ensure!(said(&v, &slow, "paused").0, "{v}");
    // 12 s paused in all, past what's left of slow's 15 s.
    std::thread::sleep(Duration::from_secs(12).saturating_sub(paused_at.elapsed()));

    let (st, v) = post(&s, &format!("/api/runs/{v1_run}/resume"), &json!({ "jobs": [first] }))?;
    anyhow::ensure!(st == 200 && said(&v, &first, "resumed").0, "v1 resume: {st} {v}");
    let (st, v) = post(&s, &format!("/api/runs/{v2_run}/resume"), &json!({ "jobs": [slow] }))?;
    anyhow::ensure!(st == 200 && said(&v, &slow, "resumed").0, "the engine's resume: {st} {v}");
    anyhow::ensure!(v1(&s)?.get("first").is_some_and(|j| j.get("paused_at").is_none()), "first no longer reads paused");
    // Both run on and finish: slow passes though its step took longer than 15 s all told.
    until(&s, 60, "first and slow to finish", || {
        let (f, sl) = (v1(&s)?.get("first").cloned().unwrap_or_default(), v2(&s)?.get("slow").cloned().unwrap_or_default());
        let done = text(&f, "status") == "succeeded" && (text(&sl, "conclusion") == "success" || text(&sl, "status") == "succeeded");
        Ok(if done { Ok(()) } else { Err(format!("first {f}, slow {sl}")) })
    })?;
    anyhow::ensure!((ticks(&marks, "v1"), ticks(&marks, "v2")) == (10, 10), "every tick came");
    anyhow::ensure!(paused_at.elapsed() + Duration::from_secs(2) > Duration::from_secs(15), "slow ran past its 15 s, paused");

    // then, paused while it waited, isn't dispatched though first is done; resumed, it runs.
    std::thread::sleep(Duration::from_secs(4));
    let t = v1(&s)?.get("then").cloned().unwrap_or_default();
    anyhow::ensure!(text(&t, "status") == "queued" && t.get("paused_at").is_some_and(Value::is_i64) && !marks.join("then").exists(), "then waits, paused: {t}");
    let (st, v) = post(&s, &format!("/api/runs/{v1_run}/resume"), &json!({ "jobs": [then] }))?;
    anyhow::ensure!(st == 200 && said(&v, &then, "resumed").0, "{st} {v}");
    until(&s, 60, "then to run once resumed", || {
        let t = v1(&s)?.get("then").cloned().unwrap_or_default();
        Ok(if text(&t, "status") == "succeeded" && marks.join("then").exists() { Ok(()) } else { Err(t.to_string()) })
    })?;
    // A job that has ended can't be paused, and says so.
    let (_, v) = post(&s, &format!("/api/runs/{v1_run}/pause"), &json!({ "jobs": [first] }))?;
    anyhow::ensure!(!said(&v, &first, "paused").0 && said(&v, &first, "paused").1.contains("has ended"), "{v}");
    // Without jobs named: a bad request.
    let (st, _) = post(&s, &format!("/api/runs/{v1_run}/pause"), &json!({}))?;
    anyhow::ensure!(st == 400, "pause with no jobs: {st}");
    Ok(())
}
