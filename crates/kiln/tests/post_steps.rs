//! W06-AC05 and AC06 (docs/ci.md) against a server with its own runner, its actions from a source
//! of the test's own (`[[ci.actions.sources]]`):
//! - AC05: pre, main and post steps across three actions in GitHub's order, with `post-if`; a run
//!   cancelled mid-step, which runs its `always()` step and its post steps; a job past its
//!   timeout, `timed_out`, its dependant seeing `failure`.
//! - AC06: in a job container, GitHub's in-container paths, and the file commands working at them;
//!   in a Docker action, `/github/workspace` and `/github/file_commands`.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, http, start};
use serde_json::{Value, json};

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let o = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(())
}

/// `files` committed in `dir` (made a repository first), tagged `tag` when given.
fn commit(dir: &Path, files: &[(&str, &str)], tag: Option<&str>) -> anyhow::Result<()> {
    if !dir.join(".git").exists() {
        std::fs::create_dir_all(dir)?;
        git(dir, &["init", "-q", "-b", "main"])?;
    }
    for (path, text) in files {
        let p = dir.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, text)?;
    }
    git(dir, &["add", "-A"])?;
    git(dir, &["commit", "-qm", "change"])?;
    if let Some(t) = tag {
        git(dir, &["tag", "-f", t])?;
    }
    Ok(())
}

fn get(s: &common::Server, path: &str) -> anyhow::Result<Value> {
    let (_, body) = http(s.port, "GET", path, &s.admin, None)?;
    Ok(serde_json::from_str(&body)?)
}

fn docker_works() -> bool {
    Command::new("docker").args(["info", "--format", "{{.ServerVersion}}"]).output().is_ok_and(|o| o.status.success())
}

/// A JavaScript action whose pre, main and post steps say which they are and whose they are.
fn tell(post_if: Option<&str>) -> Vec<(&'static str, String)> {
    let post_if = post_if.map(|c| format!("  post-if: {c}\n")).unwrap_or_default();
    vec![
        ("action.yml", format!("name: tell\ninputs:\n  name:\n    description: whose\n    required: true\nruns:\n  using: node24\n  pre: pre.js\n  main: main.js\n  post: post.js\n{post_if}")),
        ("pre.js", "console.log(`tell pre ${process.env.INPUT_NAME}`);\n".into()),
        ("main.js", "console.log(`tell main ${process.env.INPUT_NAME}`);\n".into()),
        ("post.js", "console.log(`tell post ${process.env.INPUT_NAME}`);\n".into()),
    ]
}

/// A Docker action that writes where GitHub's Docker actions see the workspace and the file
/// commands, and says where they are.
const PATHS: &[(&str, &str)] = &[
    ("action.yml", "name: paths\noutputs:\n  where:\n    description: w\nruns:\n  using: docker\n  image: Dockerfile\n"),
    ("Dockerfile", "FROM alpine:3.20\nCOPY main.sh /\nRUN chmod +x /main.sh\nENTRYPOINT [\"/main.sh\"]\n"),
    (
        "main.sh",
        "#!/bin/sh\necho \"action workspace=$GITHUB_WORKSPACE output=$GITHUB_OUTPUT\"\necho \"where=$(pwd)\" >> \"$GITHUB_OUTPUT\"\necho from-the-action > \"$GITHUB_WORKSPACE/from-action.txt\"\n",
    ),
];

const POST: &str = r#"on:
  push:
    branches: [main]
jobs:
  order:
    runs-on: local
    steps:
      - uses: kinds/tell@v1
        with:
          name: one
      - uses: kinds/tell-on-success@v1
        with:
          name: two
      - uses: kinds/tell@v1
        with:
          name: three
      - run: exit 1
      - if: always()
        run: echo "always ran"
  late:
    runs-on: local
    timeout-minutes: 1
    steps:
      - run: echo late-started; sleep 300
  after:
    needs: late
    if: always()
    runs-on: local
    steps:
      - run: echo "needs said ${{ needs.late.result }}"
"#;

const CANCEL: &str = r#"on:
  push:
    branches: [cancel]
jobs:
  stopped:
    runs-on: local
    steps:
      - uses: kinds/tell@v1
        with:
          name: four
      - run: echo started-sleeping; sleep 300
      - if: always()
        run: echo "always after the cancel"
      - run: echo "never after the cancel"
"#;

const PATHS_WF: &str = r###"on:
  push:
    branches: [paths]
jobs:
  inside:
    runs-on: local
    container: alpine:3.20
    steps:
      - id: where
        run: |
          echo "workspace=$GITHUB_WORKSPACE temp=$RUNNER_TEMP tools=$RUNNER_TOOL_CACHE"
          echo "commands=$(dirname "$GITHUB_OUTPUT") $(dirname "$GITHUB_ENV") $(dirname "$GITHUB_PATH") $(dirname "$GITHUB_STEP_SUMMARY")"
          echo "said=hello" >> "$GITHUB_OUTPUT"
          echo "FROM_ENV=set" >> "$GITHUB_ENV"
          echo /opt/from-path >> "$GITHUB_PATH"
          echo "## summed" >> "$GITHUB_STEP_SUMMARY"
      - id: act
        uses: kinds/paths@v1
      - run: |
          echo "output=${{ steps.where.outputs.said }} env=$FROM_ENV path=$PATH"
          echo "action=${{ steps.act.outputs.where }} file=$(cat from-action.txt)"
"###;

/// A server with its own runner whose `kinds/*` actions come from `src`, polling `repo`, once it
/// has taken the repository as it is.
fn server_for(dir: &Path, src: &Path, repo: &Path) -> anyhow::Result<common::Server> {
    commit(repo, &[("README.md", "kinds\n")], None)?;
    // The machine's Node 24, when it has one, so the runner needn't fetch it.
    if let Some(node) = std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("node")).find(|n| n.is_file()))
        && Command::new(&node).arg("--version").output().is_ok_and(|v| String::from_utf8_lossy(&v.stdout).starts_with("v24."))
    {
        let bin = dir.join("work/_externals/node24/bin");
        std::fs::create_dir_all(&bin)?;
        std::os::unix::fs::symlink(&node, bin.join("node"))?;
    }
    let ci = format!("\n[[ci.actions.sources]]\nowners = [\"kinds\"]\nurl = \"file://{}/{{owner}}/{{repo}}\"\n", src.display());
    let extra = format!("[[repos]]\nname = \"acme/kinds\"\nclone_url = \"file://{}\"\nbranches = [\"main\", \"cancel\", \"paths\"]\npoll_seconds = 1\n", repo.display());
    let s = start(dir, &Options { agent: true, ci: &ci, extra: &extra })?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        if body.contains("acme/kinds") && !body.contains("\"last_polled_at\":null") {
            return Ok(s);
        }
        anyhow::ensure!(Instant::now() < deadline, "never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// The newest run on `branch`, once `done` says it's where the test wants it.
fn run_on(s: &common::Server, branch: &str, secs: u64, done: impl Fn(&Value) -> bool) -> anyhow::Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let runs = get(s, "/api/ci/runs?repo=acme/kinds&limit=20")?;
        if let Some(r) = runs.get("runs").and_then(Value::as_array).into_iter().flatten().find(|r| r.get("branch").and_then(Value::as_str) == Some(branch)).filter(|r| done(r)) {
            return Ok(r.clone());
        }
        anyhow::ensure!(Instant::now() < deadline, "no run on {branch} as wanted: {runs}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// A run's legs by job, each with its steps' names, conclusions and logs in the order they ran.
fn legs(s: &common::Server, run: &str) -> anyhow::Result<Vec<(String, String, Vec<(String, String, String)>)>> {
    let detail = get(s, &format!("/api/ci/runs/{run}"))?;
    let mut out = Vec::new();
    for l in detail.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
        let id = l.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let leg = get(s, &format!("/api/ci/legs/{id}"))?;
        let mut steps: Vec<Value> = leg.get("steps").and_then(Value::as_array).cloned().unwrap_or_default();
        steps.sort_by_key(|x| x.get("seq").and_then(Value::as_i64).unwrap_or(i64::MAX));
        let mut ran = Vec::new();
        for x in &steps {
            let idx = x.get("idx").and_then(Value::as_i64).unwrap_or_default();
            let (_, log) = http(s.port, "GET", &format!("/api/ci/legs/{id}/logs?step={idx}"), &s.admin, None)?;
            ran.push((x.get("name").and_then(Value::as_str).unwrap_or_default().to_owned(), x.get("conclusion").and_then(Value::as_str).unwrap_or_default().to_owned(), log));
        }
        let key = l.get("key").and_then(Value::as_str).unwrap_or_default().to_owned();
        let conclusion = l.get("conclusion").and_then(Value::as_str).unwrap_or_default().to_owned();
        out.push((key, conclusion, ran));
    }
    Ok(out)
}

fn said(steps: &[(String, String, String)]) -> String {
    steps.iter().map(|(n, c, l)| format!("--- {n} ({c})\n{l}")).collect::<Vec<_>>().join("\n")
}

/// W06-AC05.
#[test]
fn w06_ac05_post_steps_cancellation_and_timeouts() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("src");
    let t = tell(None);
    commit(&src.join("kinds/tell"), &t.iter().map(|(p, x)| (*p, x.as_str())).collect::<Vec<_>>(), Some("v1"))?;
    let t = tell(Some("success()"));
    commit(&src.join("kinds/tell-on-success"), &t.iter().map(|(p, x)| (*p, x.as_str())).collect::<Vec<_>>(), Some("v1"))?;
    let repo = dir.path().join("repo");
    let s = server_for(dir.path(), &src, &repo)?;
    commit(&repo, &[(".github/workflows/post.yml", POST), (".github/workflows/cancel.yml", CANCEL)], None)?;
    let run = run_on(&s, "main", 300, |r| r.get("finished_at").is_some_and(|f| !f.is_null()))?;
    let all = legs(&s, run.get("id").and_then(Value::as_str).unwrap_or_default())?;
    let job = |k: &str| all.iter().find(|(key, _, _)| key == k).cloned().unwrap_or_default();

    // Pre steps in step order, then the main steps, then the posts in reverse, those whose
    // `post-if` holds: `success()` doesn't once a step failed.
    let (_, conclusion, steps) = job("order");
    let log = said(&steps);
    assert_eq!(conclusion, "failure", "{log}");
    let at = |what: &str| log.find(what).ok_or_else(|| anyhow::anyhow!("{what:?} in no step:\n{log}"));
    let order = ["tell pre one", "tell pre two", "tell pre three", "tell main one", "tell main two", "tell main three", "always ran", "tell post three", "tell post one"];
    let places: Vec<usize> = order.iter().map(|w| at(w)).collect::<anyhow::Result<_>>()?;
    assert!(places.windows(2).all(|w| w.first() < w.get(1)), "GitHub's order {order:?}:\n{log}");
    assert!(!log.contains("tell post two"), "post-if: success() after a failure:\n{log}");

    // A job past its timeout is cancelled as `timed_out`; its dependant sees `failure`.
    let (_, conclusion, steps) = job("late");
    assert_eq!(conclusion, "timed_out", "{}", said(&steps));
    assert!(said(&steps).contains("has exceeded the maximum execution time of 1 minutes"), "{}", said(&steps));
    let (_, conclusion, steps) = job("after");
    assert_eq!(conclusion, "success", "{}", said(&steps));
    assert!(said(&steps).contains("needs said failure"), "{}", said(&steps));

    // Cancelled mid-step: the step is cancelled, the `always()` step runs, the next one doesn't,
    // and the posts run.
    git(&repo, &["checkout", "-q", "-b", "cancel"])?;
    commit(&repo, &[("cancel.txt", "go\n")], None)?;
    let run = run_on(&s, "cancel", 180, |_| true)?;
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    // Its sleeping step running (a step's log comes once it ends).
    let deadline = Instant::now() + Duration::from_secs(120);
    let sleeping = |s: &common::Server| -> anyhow::Result<bool> {
        let detail = get(s, &format!("/api/ci/runs/{id}"))?;
        let Some(leg) = detail.pointer("/legs/0/id").and_then(Value::as_str) else { return Ok(false) };
        let leg = get(s, &format!("/api/ci/legs/{leg}"))?;
        Ok(leg
            .get("steps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|x| x.get("name").and_then(Value::as_str).is_some_and(|n| n.contains("sleep 300")) && x.get("status").and_then(Value::as_str) == Some("running")))
    };
    while !sleeping(&s)? {
        anyhow::ensure!(Instant::now() < deadline, "never started sleeping: {:?}", legs(&s, &id)?);
        std::thread::sleep(Duration::from_millis(300));
    }
    std::thread::sleep(Duration::from_secs(2));
    let (st, body) = http(s.port, "POST", &format!("/api/ci/runs/{id}/cancel"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!((200..300).contains(&st), "cancel: {st} {body}");
    let run = run_on(&s, "cancel", 120, |r| r.get("finished_at").is_some_and(|f| !f.is_null()))?;
    assert_eq!(run.get("conclusion").and_then(Value::as_str), Some("cancelled"), "{run}");
    let all = legs(&s, &id)?;
    let (_, conclusion, steps) = all.first().cloned().unwrap_or_default();
    let log = said(&steps);
    assert_eq!(conclusion, "cancelled", "{log}");
    let sleeping = steps.iter().find(|(_, _, l)| l.contains("started-sleeping")).map(|(_, c, _)| c.clone());
    assert_eq!(sleeping.as_deref(), Some("cancelled"), "{log}");
    let step = |what: &str| steps.iter().find(|(n, _, _)| n.contains(what)).cloned().unwrap_or_default();
    let (_, ran, out) = step("always after");
    assert!(ran == "success" && out.contains("always after the cancel"), "the `always()` step runs to its end:\n{log}");
    let (_, skipped, out) = step("never after");
    assert!(skipped == "skipped" && out.is_empty(), "the next one doesn't:\n{log}");
    assert!(log.contains("tell post four"), "its post step:\n{log}");
    Ok(())
}

/// W06-AC06.
#[test]
fn w06_ac06_paths_in_containers() -> anyhow::Result<()> {
    if !docker_works() {
        eprintln!("w06_ac06_paths_in_containers: skipped, no Docker");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("src");
    commit(&src.join("kinds/paths"), PATHS, Some("v1"))?;
    let repo = dir.path().join("repo");
    let s = server_for(dir.path(), &src, &repo)?;
    git(&repo, &["checkout", "-q", "-b", "paths"])?;
    commit(&repo, &[(".github/workflows/paths.yml", PATHS_WF)], None)?;
    let run = run_on(&s, "paths", 600, |r| r.get("finished_at").is_some_and(|f| !f.is_null()))?;
    let all = legs(&s, run.get("id").and_then(Value::as_str).unwrap_or_default())?;
    let (_, conclusion, steps) = all.first().cloned().unwrap_or_default();
    let log = said(&steps);
    assert_eq!(conclusion, "success", "{log}\n{}", s.log());
    for want in [
        // GitHub's in-container paths.
        "workspace=/__w/kinds/kinds temp=/__w/_temp tools=/__t",
        "commands=/__w/_temp/_runner_file_commands /__w/_temp/_runner_file_commands /__w/_temp/_runner_file_commands /__w/_temp/_runner_file_commands",
        // The file commands, at them.
        "output=hello env=set path=/opt/from-path:",
        // A Docker action's.
        "action workspace=/github/workspace output=/github/file_commands/",
        "action=/github/workspace file=from-the-action",
    ] {
        assert!(log.contains(want), "{want:?}:\n{log}");
    }
    Ok(())
}
