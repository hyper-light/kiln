//! Sharing across repositories (docs/ci.md W29-R17 to R20), against a server of its own with its
//! own runner and real `git`: who may use a repository's actions and reusable workflows, as its
//! Access says.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, Server, http, start};
use serde_json::{Value, json};

/// `git` with no configuration but this test's, in `dir`.
fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git")
        .args(["-c", "user.name=Ada", "-c", "user.email=ada@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper=", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    anyhow::ensure!(o.status.success(), "git {args:?}: {out}");
    Ok(out)
}

fn get(s: &Server, path: &str) -> anyhow::Result<Value> {
    let (st, text) = http(s.port, "GET", path, &s.admin, None)?;
    anyhow::ensure!(st == 200, "GET {path}: {st} {text}");
    Ok(serde_json::from_str(&text)?)
}

fn at<'a>(v: &'a Value, pointer: &str) -> &'a str {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or_default()
}

/// Until `name`'s first poll is done: it takes the branch as it is.
fn polled(s: &Server, name: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let v = get(s, "/api/repos")?;
        let list = v.as_array().cloned().or_else(|| v.get("repos").and_then(Value::as_array).cloned()).unwrap_or_default();
        if list.iter().any(|r| r.get("name").and_then(Value::as_str) == Some(name) && r.get("last_polled_at").is_some_and(|t| !t.is_null())) {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "{name} wasn't polled: {v}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Writes `files` under `work`.
fn write(work: &Path, files: &[(&str, &str)]) -> anyhow::Result<()> {
    for (path, text) in files {
        let p = work.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, text)?;
    }
    Ok(())
}

/// A repository kiln hosts, cloned under `dir` (its `/` a `_`), its first commit (a README and
/// `files`) pushed and polled.
fn hosted(s: &Server, home: &Path, dir: &Path, name: &str, files: &[(&str, &str)]) -> anyhow::Result<PathBuf> {
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": name, "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{name}: {st} {body}");
    let leaf = name.replace('/', "_");
    git(home, dir, &["clone", "-q", &format!("http://x:{}@127.0.0.1:{}/git/{name}.git", s.admin, s.port), &leaf])?;
    let work = dir.join(&leaf);
    write(&work, &[("README.md", &format!("{name}\n"))])?;
    write(&work, files)?;
    git(home, &work, &["add", "."])?;
    git(home, &work, &["commit", "-qm", "first"])?;
    git(home, &work, &["push", "-q", "origin", "main"])?;
    polled(s, name)?;
    Ok(work)
}

/// Writes `files` in `work`, commits them as `message` and pushes main: the commit.
fn push(home: &Path, work: &Path, message: &str, files: &[(&str, &str)]) -> anyhow::Result<String> {
    write(work, files)?;
    git(home, work, &["add", "."])?;
    git(home, work, &["commit", "-qm", message])?;
    git(home, work, &["push", "-q", "origin", "main"])?;
    Ok(git(home, work, &["rev-parse", "HEAD"])?.trim().to_owned())
}

/// `repo`'s runs at `sha`, once `n` of them have finished.
fn finished(s: &Server, repo: &str, sha: &str, n: usize) -> anyhow::Result<Vec<Value>> {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let v = get(s, &format!("/api/ci/runs?repo={repo}&limit=100"))?;
        let all: Vec<Value> = v.get("runs").and_then(Value::as_array).into_iter().flatten().filter(|r| at(r, "/sha") == sha).cloned().collect();
        let done = all.iter().filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())).count();
        if done >= n {
            return Ok(all);
        }
        anyhow::ensure!(Instant::now() < deadline, "{repo} at {sha}: {done} of {n} runs finished: {all:?}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// A run refused as it starts (W29-R19): KCI4022, naming who it isn't shared with, and no job.
fn refused(s: &Server, run: &Value, caller: &str, what: &str) -> anyhow::Result<()> {
    assert_eq!(at(run, "/conclusion"), "startup_failure", "{what}: {run}");
    let why = at(run, "/error");
    assert!(why.contains("KCI4022") && why.contains(&format!("isn't shared with `{caller}`")), "{what}: {run}");
    let detail = get(s, &format!("/api/ci/runs/{}", at(run, "/id")))?;
    assert_eq!(detail.get("legs").and_then(Value::as_array).map_or(0, Vec::len), 0, "{what}: no job ran: {detail}");
    Ok(())
}

/// acme/kit's composite action and reusable workflow.
const ACTION: &str = "name: x\nruns:\n  using: composite\n  steps:\n    - run: echo shared\n      shell: bash\n";
const BUILD: &str = "name: build\non: workflow_call\njobs:\n  b:\n    runs-on: local\n    steps:\n      - run: echo built\n";

/// A caller's workflows, one use of acme/kit each: a step's action and a job's reusable workflow,
/// through `kiln://` and through a plain name (`[[ci.actions.sources]]` sends acme's to kiln).
fn callers_workflows() -> Vec<(&'static str, String)> {
    let step = |u: &str| format!("on: push\njobs:\n  a:\n    runs-on: local\n    steps:\n      - uses: {u}\n");
    let call = |u: &str| format!("on: push\njobs:\n  c:\n    uses: {u}\n");
    vec![
        (".kiln/step-kiln.yaml", step("kiln://acme/kit/.github/actions/x@main")),
        (".kiln/step-plain.yaml", step("acme/kit/.github/actions/x@main")),
        (".kiln/call-kiln.yaml", call("kiln://acme/kit/.github/workflows/build.yml@main")),
        (".kiln/call-plain.yaml", call("acme/kit/.github/workflows/build.yml@main")),
    ]
}

/// W29-R19: one check, for actions and reusable workflows alike. acme/kit's action and reusable
/// workflow, used from acme/app and from other/app through `kiln://` and through a plain name a
/// native source sends to kiln: not shared, only acme/kit's own workflows may use them; `share:
/// owner`, acme's other repositories too; `share: everyone`, everyone's. A refused use fails its
/// run as it starts, before any job, with KCI4022, the reusable workflow as the action.
#[test]
fn w29_r19_actions_and_reusable_workflows_alike() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let ci = "kiln_workflows = true\n[[ci.actions.sources]]\nowners = [\"acme\"]\nnative = true\n";
    let s = start(dir.path(), &Options { agent: true, ci, extra: "" })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let kit = hosted(&s, &home, dir.path(), "acme/kit", &[(".github/actions/x/action.yml", ACTION), (".github/workflows/build.yml", BUILD)])?;
    let callers = ["acme/app", "other/app"];
    let mut works = Vec::new();
    for name in callers {
        works.push(hosted(&s, &home, dir.path(), name, &[])?);
    }

    // Its own workflows always may, not shared as it is.
    let own = "on: push\njobs:\n  a:\n    runs-on: local\n    steps:\n      - uses: kiln://acme/kit/.github/actions/x@main\n      - uses: acme/kit/.github/actions/x@main\n  c:\n    uses: kiln://acme/kit/.github/workflows/build.yml@main\n";
    let sha = push(&home, &kit, "its own", &[(".kiln/own.yaml", own)])?;
    let runs = finished(&s, "acme/kit", &sha, 1)?;
    assert_eq!(runs.iter().map(|r| at(r, "/conclusion")).collect::<Vec<_>>(), ["success"], "{runs:?}");

    let workflows = callers_workflows();
    // Each level, and the callers it lets use acme/kit.
    let levels: [(&str, &[&str]); 3] = [("none", &[]), ("owner", &["acme/app"]), ("everyone", &["acme/app", "other/app"])];
    for (level, may) in levels {
        if level != "none" {
            push(&home, &kit, &format!("share {level}"), &[(".kiln/config.yaml", &format!("share: {level}\n"))])?;
            // acme/kit's settings are read from its trunk and kept 20 seconds (repo_config): the
            // change is waited out.
            std::thread::sleep(Duration::from_secs(21));
        }
        for (name, work) in callers.iter().zip(&works) {
            let readme = format!("{name}, acme/kit shared {level}\n");
            let mut files: Vec<(&str, &str)> = workflows.iter().map(|(p, t)| (*p, t.as_str())).collect();
            files.push(("README.md", &readme));
            let sha = push(&home, work, &format!("uses acme/kit, shared {level}"), &files)?;
            let runs = finished(&s, name, &sha, workflows.len())?;
            for (path, _) in &workflows {
                let what = format!("{name}'s {path}, acme/kit shared {level}");
                let run = runs.iter().find(|r| at(r, "/workflow") == *path).cloned().unwrap_or_default();
                if may.contains(name) {
                    assert_eq!(at(&run, "/conclusion"), "success", "{what}: {run}\n{}", s.log());
                } else {
                    refused(&s, &run, name, &what)?;
                }
            }
        }
    }
    Ok(())
}
