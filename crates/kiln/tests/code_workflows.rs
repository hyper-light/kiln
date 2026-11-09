//! W29-R27 (docs/ci.md): code workflows end to end, against a server of its own with real `git`.
//! A `.kiln/ci.ts` pushed to a repository kiln hosts is planned from its program, made for each
//! event: a push, a pull request (which adds a job), a dispatch (whose inputs reach the program as
//! `event.inputs`) and a schedule. The editor's check puts W02's errors at the program's lines,
//! plan-preview says the YAML the program makes, and a push starts the run its program makes.

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

fn post(s: &Server, path: &str, body: &Value) -> anyhow::Result<Value> {
    let (st, text) = http(s.port, "POST", path, &s.admin, Some(body))?;
    anyhow::ensure!(st == 200, "{path}: {st} {text}");
    Ok(serde_json::from_str(&text)?)
}

/// The jobs a plan's legs are of.
fn jobs(plan: &Value) -> Vec<String> {
    let mut out: Vec<String> = plan.pointer("/plan/legs").and_then(Value::as_array).into_iter().flatten().filter_map(|l| l.get("job").and_then(Value::as_str)).map(str::to_owned).collect();
    out.sort();
    out.dedup();
    out
}

const CI: &str = r#"import { workflow, job, step } from "@kiln/sdk";

type Event = { pull_request?: unknown; inputs?: { target?: string } };

export default (event: Event) =>
  workflow({
    name: "code",
    on: {
      push: { branches: ["main"] },
      pull_request: null,
      workflow_dispatch: { inputs: { target: { type: "string", default: "all" } } },
      schedule: [{ cron: "0 3 * * *" }],
    },
    jobs: {
      build: job({ "runs-on": "local", steps: [step({ run: `echo build ${event.inputs?.target ?? "push"}` })] }),
      ...(event.pull_request === undefined ? {} : { review: job({ "runs-on": "local", steps: [step({ run: "echo review" })] }) }),
    },
  });
"#;

#[test]
fn w29_r27_a_code_workflow_is_planned_for_each_event() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { ci: "kiln_workflows = true\n", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/code", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/code.git", s.admin, s.port);
    git(&home, dir.path(), &["clone", "-q", &url, "work"])?;
    let work = dir.path().join("work");
    // The poller adopts what it first sees and runs from the next change on: a first commit.
    std::fs::write(work.join("README.md"), "code\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        if !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".kiln/ci.ts"), CI)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "code"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;
    let sha = git(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();
    let preview = |event: Value| post(&s, "/api/ci/plan-preview", &json!({ "repo": "acme/code", "ref": "main", "sha": sha, "path": ".kiln/ci.ts", "event": event }));

    // A push: the program's one job, and the YAML it makes, GitHub's keys only.
    let push = preview(json!("push"))?;
    assert_eq!((push.get("runs"), jobs(&push)), (Some(&json!(true)), vec!["build".to_owned()]), "{push}");
    let yaml = push.get("emitted").and_then(Value::as_str).unwrap_or_default();
    assert!(yaml.starts_with("name: code\n") && yaml.contains("echo build push"), "{yaml}");
    assert_eq!(push.get("emitted_github"), Some(&json!(true)), "{push}");
    // What it makes plans as the program does, and reads back with no error.
    let back = post(&s, "/api/ci/plan-preview", &json!({ "repo": "acme/code", "ref": "main", "sha": sha, "path": ".kiln/made.yaml", "content": yaml, "event": "push" }))?;
    let errors = back.get("diagnostics").and_then(Value::as_array).into_iter().flatten().filter(|d| d.get("severity") == Some(&json!("error"))).count();
    assert_eq!((jobs(&back), errors), (jobs(&push), 0), "{back}");
    // A pull request: the program adds `review`.
    let pr = preview(json!("pull_request"))?;
    assert_eq!(jobs(&pr), vec!["build".to_owned(), "review".to_owned()], "{pr}");
    // A dispatch: its input reaches the program, typed as the program declares it.
    let dispatched = preview(json!({ "name": "workflow_dispatch", "inputs": { "target": "docs" } }))?;
    assert!(dispatched.get("emitted").and_then(Value::as_str).is_some_and(|y| y.contains("echo build docs")), "{dispatched}");
    // A schedule: its cron, as W03 reads it.
    let schedule = preview(json!("schedule"))?;
    assert!(schedule.pointer("/schedules/0/cron") == Some(&json!("0 3 * * *")), "{schedule}");

    // The editor's check: W02's error at the line the program wrote it.
    let wrong = CI.replace("build: job({ \"runs-on\": \"local\", steps:", "build: job({ \"runs-on\": \"local\", stepz:");
    let checked = post(&s, "/api/ci/validate", &json!({ "path": ".kiln/ci.ts", "content": wrong, "repo": "acme/code" }))?;
    let errors: Vec<u64> = checked.get("diagnostics").and_then(Value::as_array).into_iter().flatten().filter(|d| d.get("severity") == Some(&json!("error"))).filter_map(|d| d.pointer("/range/start/line").and_then(Value::as_u64)).collect();
    assert!(!errors.is_empty() && errors.iter().all(|l| *l == 15), "{checked}");
    let clean = post(&s, "/api/ci/validate", &json!({ "path": ".kiln/ci.ts", "content": CI, "repo": "acme/code" }))?;
    assert!(clean.get("diagnostics").and_then(Value::as_array).is_some_and(|d| d.iter().all(|d| d.get("severity") != Some(&json!("error")))), "{clean}");

    // The push's run, and a dispatch's: the jobs the program made for each.
    assert_eq!(ran(&s, &sha, "push")?, vec!["build".to_owned()]);
    let (st, body) = http(s.port, "POST", "/api/ci/repos/acme%2Fcode/workflows/.kiln/ci.ts/dispatches", &s.admin, Some(&json!({ "ref": "main", "inputs": { "target": "docs" } })))?;
    anyhow::ensure!((200..300).contains(&st), "dispatch: {st} {body}");
    assert_eq!(ran(&s, &sha, "workflow_dispatch")?, vec!["build".to_owned()]);
    Ok(())
}

/// The jobs of the run of `.kiln/ci.ts` at `sha` for `event`, once it's there.
fn ran(s: &Server, sha: &str, event: &str) -> anyhow::Result<Vec<String>> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let run = loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/code&limit=20", &s.admin, None)?;
        let runs: Value = serde_json::from_str(&body).unwrap_or_default();
        let found = runs.get("runs").and_then(Value::as_array).into_iter().flatten().find(|r| r.get("sha") == Some(&json!(sha)) && r.get("event") == Some(&json!(event))).and_then(|r| r.get("id").and_then(Value::as_str)).map(str::to_owned);
        if let Some(id) = found {
            break id;
        }
        anyhow::ensure!(Instant::now() < deadline, "no {event} run for {sha}: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
    let detail: Value = serde_json::from_str(&detail)?;
    let mut jobs: Vec<String> = detail.get("legs").and_then(Value::as_array).into_iter().flatten().filter_map(|l| l.get("job").and_then(Value::as_str)).map(str::to_owned).collect();
    jobs.sort();
    jobs.dedup();
    Ok(jobs)
}
