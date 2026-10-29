//! W05-AC12 (docs/ci.md W05-R28): a finished job run again elsewhere, or on a runner named, end
//! to end on a server with two runners carrying the same label: a `.kiln/` pipeline's job (v1)
//! and a leg of the CI engine's, each failing wherever it runs. Retried elsewhere, each runs on
//! the other runner; retried on a runner named, on that one; the runners each could run on are
//! listed with the one it ran on; and what can't be asked is refused, saying why.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{Options, Server, http, start};
use serde_json::{Value, json};

struct Runner(Child);

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let o = Command::new("git").arg("-C").arg(dir).args(args).env("GIT_AUTHOR_NAME", "T").env("GIT_AUTHOR_EMAIL", "t@example.com").env("GIT_COMMITTER_NAME", "T").env("GIT_COMMITTER_EMAIL", "t@example.com").output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(())
}

/// A runner of its own named `name`, carrying the label `shared`.
fn runner(s: &Server, dir: &Path, join: &str, name: &str) -> anyhow::Result<Runner> {
    let home = dir.join(format!("{name}-home"));
    std::fs::create_dir_all(&home)?;
    let out = std::fs::File::create(dir.join(format!("{name}.log")))?;
    Ok(Runner(
        Command::new(env!("CARGO_BIN_EXE_kiln"))
            .args(["agent", "--server", &format!("http://127.0.0.1:{}", s.port), "--token", join, "--name", name, "--labels", "shared", "--slots", "1", "--no-terminal"])
            .arg("--work-dir")
            .arg(dir.join(format!("{name}-work")))
            .arg("--state-dir")
            .arg(dir.join(format!("{name}-state")))
            .env("HOME", home)
            .env("NO_COLOR", "1")
            .env_remove("KILN_CONFIG")
            .env_remove("KILN_CHAOS")
            .env_remove("KILN_REACH_SOCK")
            .stdout(Stdio::from(out.try_clone()?))
            .stderr(Stdio::from(out))
            .spawn()?,
    ))
}

fn get(s: &Server, path: &str) -> anyhow::Result<Value> {
    let (_, body) = http(s.port, "GET", path, &s.admin, None)?;
    Ok(serde_json::from_str(&body).unwrap_or_default())
}

/// Waits until `f` has what's waited for (Some), at most `secs`.
fn until<T>(s: &Server, secs: u64, what: &str, mut f: impl FnMut() -> anyhow::Result<Result<T, String>>) -> anyhow::Result<T> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match f()? {
            Ok(v) => return Ok(v),
            Err(why) => anyhow::ensure!(Instant::now() < deadline, "waited {secs}s for {what}: {why}\n--- the server:\n{}", s.log()),
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

const FINISHED: [&str; 4] = ["succeeded", "failed", "cancelled", "timed_out"];

#[test]
fn w05_ac12_a_failed_job_retried_elsewhere_and_on_a_runner_named() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::create_dir_all(repo.join(".github/workflows"))?;
    // A pipeline's job and a leg of the engine's, each failing wherever it runs.
    std::fs::write(repo.join(".kiln/pipeline.yaml"), "name: v1\non:\n  push:\n    branches: [main]\njobs:\n  fails:\n    runs-on: [shared]\n    steps:\n      - run: echo \"v1 here\" && exit 1\n")?;
    std::fs::write(repo.join(".github/workflows/v2.yml"), "name: v2\non: push\njobs:\n  fails:\n    runs-on: [self-hosted, shared]\n    steps:\n      - run: echo \"v2 here\" && exit 1\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!("[[repos]]\nname = \"acme/retry\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    // GitHub's workflows run beside `.kiln/` pipelines, which stay on v1.
    let s = start(dir, &Options { agent: false, ci: "github_workflows = \"on\"", extra: &extra })?;
    let join = std::fs::read_to_string(dir.join("data/join.token"))?.trim().to_owned();
    let _one = runner(&s, dir, &join, "one")?;
    let _two = runner(&s, dir, &join, "two")?;
    until(&s, 120, "both runners and the repository's first poll", || {
        let agents = get(&s, "/api/agents")?;
        let list = agents.as_array().cloned().or_else(|| agents.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
        let up = ["one", "two"].iter().all(|n| list.iter().any(|a| text(a, "name") == *n && a.get("connected") == Some(&json!(true))));
        let (_, repos) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(if up && repos.contains("acme/retry") && !repos.contains("\"last_polled_at\":null") { Ok(()) } else { Err(format!("{agents}\n{repos}")) })
    })?;
    std::fs::write(repo.join("README.md"), "retry\n")?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "go"])?;
    let other = |r: &str| if r == "one" { "two" } else { "one" };

    // v1: the pipeline's job fails on a runner.
    let job = until(&s, 300, "the pipeline's job to fail", || {
        let runs = get(&s, "/api/runs?repo=acme/retry")?;
        let list = runs.as_array().cloned().or_else(|| runs.get("runs").and_then(Value::as_array).cloned()).unwrap_or_default();
        for r in list.iter().filter(|r| text(r, "pipeline") == "v1") {
            let detail = get(&s, &format!("/api/runs/{}", text(r, "id")))?;
            if let Some(j) = detail.get("jobs").and_then(Value::as_array).and_then(|j| j.iter().find(|j| text(j, "key") == "fails" && text(j, "status") == "failed")) {
                return Ok(Ok(j.clone()));
            }
        }
        Ok(Err(runs.to_string()))
    })?;
    let (id, first) = (text(&job, "id").to_owned(), text(&job, "agent").to_owned());
    anyhow::ensure!(first == "one" || first == "two", "{job}");
    // Where it could run again: both runners, the one it ran on said.
    let options = get(&s, &format!("/api/ci/legs/{id}/runners"))?;
    assert_eq!((text(&options, "kind"), text(&options, "ran_on")), ("pipeline", first.as_str()), "{options}");
    assert_eq!((options.get("elsewhere"), options.pointer("/retry/can")), (Some(&json!(true)), Some(&json!(true))), "{options}");
    let able: Vec<&str> = options.get("runners").and_then(Value::as_array).into_iter().flatten().filter(|r| r.get("able") == Some(&json!(true))).map(|r| text(r, "name")).collect();
    assert!(able.contains(&"one") && able.contains(&"two"), "{options}");
    // What can't be asked is refused, saying why.
    let (st, body) = http(s.port, "POST", &format!("/api/ci/legs/{id}/retry"), &s.admin, Some(&json!({ "runner": "nowhere" })))?;
    assert!(st == 400 && body.contains("no runner `nowhere`"), "{st} {body}");
    let (st, body) = http(s.port, "POST", &format!("/api/ci/legs/{id}/retry"), &s.admin, Some(&json!({ "runner": "one", "elsewhere": true })))?;
    assert!(st == 400 && body.contains("not both"), "{st} {body}");
    // Elsewhere: on the other runner, the job's placement saying so.
    let (st, body) = http(s.port, "POST", &format!("/api/ci/legs/{id}/retry"), &s.admin, Some(&json!({ "elsewhere": true })))?;
    assert_eq!(st, 201, "{body}");
    let attempt = job.get("attempt").and_then(Value::as_i64).unwrap_or(0);
    let again = |n: i64| {
        until(&s, 300, "the pipeline's job's next attempt to finish", || {
            let j = get(&s, &format!("/api/jobs/{id}"))?;
            let done = j.get("attempt").and_then(Value::as_i64) == Some(n) && FINISHED.contains(&text(&j, "status"));
            Ok(if done { Ok(j) } else { Err(j.to_string()) })
        })
    };
    let j = again(attempt + 1)?;
    assert_eq!((text(&j, "agent"), j.pointer("/placement/avoid")), (other(&first), Some(&json!(first))), "{j}");
    // On a runner named, through the operation agents call (MCP's `ci.legs.retry`).
    let (st, body) = http(s.port, "POST", "/api/ops/ci.legs.retry", &s.admin, Some(&json!({ "leg": id, "runner": first })))?;
    assert_eq!(st, 200, "{body}");
    let j = again(attempt + 2)?;
    assert_eq!((text(&j, "agent"), j.pointer("/placement/runner")), (first.as_str(), Some(&json!(first))), "{j}");

    // The engine: its leg fails on a runner, and its run finishes.
    let (run, leg) = until(&s, 300, "the engine's leg to fail", || {
        let runs = get(&s, "/api/ci/runs?repo=acme/retry&limit=5")?;
        let Some(run) = runs.pointer("/runs/0/id").and_then(Value::as_str) else { return Ok(Err(runs.to_string())) };
        let detail = get(&s, &format!("/api/ci/runs/{run}"))?;
        let done = detail.pointer("/run/finished_at").is_some_and(|f| !f.is_null());
        match detail.get("legs").and_then(Value::as_array).and_then(|l| l.iter().find(|l| text(l, "key") == "fails" && text(l, "conclusion") == "failure")) {
            Some(l) if done => Ok(Ok((run.to_owned(), l.clone()))),
            _ => Ok(Err(detail.to_string())),
        }
    })?;
    let (leg_id, first) = (text(&leg, "id").to_owned(), text(&leg, "runner").to_owned());
    anyhow::ensure!(first == "one" || first == "two", "{leg}");
    let options = get(&s, &format!("/api/ci/legs/{leg_id}/runners"))?;
    assert_eq!((text(&options, "kind"), text(&options, "ran_on")), ("ci", first.as_str()), "{options}");
    let able: Vec<&str> = options.get("runners").and_then(Value::as_array).into_iter().flatten().filter(|r| r.get("able") == Some(&json!(true))).map(|r| text(r, "name")).collect();
    assert!(able.contains(&"one") && able.contains(&"two"), "{options}");
    // Elsewhere: a new attempt of the run, its leg on the other runner.
    let attempt_of = |n: i64| {
        until(&s, 300, "the run's next attempt to finish", || {
            let detail = get(&s, &format!("/api/ci/runs/{run}"))?;
            let at = detail.pointer("/run/attempt").and_then(Value::as_i64) == Some(n) && detail.pointer("/run/finished_at").is_some_and(|f| !f.is_null());
            Ok(match detail.get("legs").and_then(Value::as_array).and_then(|l| l.iter().find(|l| text(l, "key") == "fails")) {
                Some(l) if at && l.get("conclusion").is_some_and(|c| !c.is_null()) => Ok(l.clone()),
                _ => Err(detail.to_string()),
            })
        })
    };
    let (st, body) = http(s.port, "POST", &format!("/api/ci/legs/{leg_id}/retry"), &s.admin, Some(&json!({ "elsewhere": true })))?;
    assert!(st == 201 && body.contains("\"attempt\":2"), "{st} {body}");
    let l = attempt_of(2)?;
    assert_eq!(text(&l, "runner"), other(&first), "{l}");
    let placed = get(&s, &format!("/api/jobs/{}", text(&l, "id")))?;
    assert_eq!(placed.pointer("/placement/avoid"), Some(&json!(first)), "{placed}");
    // On a runner named: the next attempt's leg runs there.
    let (st, body) = http(s.port, "POST", &format!("/api/ci/legs/{}/retry", text(&l, "id")), &s.admin, Some(&json!({ "runner": first })))?;
    assert!(st == 201 && body.contains("\"attempt\":3"), "{st} {body}");
    let l = attempt_of(3)?;
    assert_eq!(text(&l, "runner"), first, "{l}");
    Ok(())
}
