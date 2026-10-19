//! W07's acceptance criteria (docs/ci.md) against a server with its own runner, its actions from a
//! source of the test's own (`[[ci.actions.sources]]`, W07-R02).
//!
//! AC04: each kind of action as on GitHub. A JavaScript action with `pre` and `post`, a Docker
//! action from a Dockerfile (with `pre-entrypoint` and `post-entrypoint`), one from `docker://`, a
//! composite three levels deep, and a local `./` action: their inputs, outputs, state and order.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, http, start};
use serde_json::Value;

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let o = Command::new("git").arg("-C").arg(dir).args(args).env("GIT_AUTHOR_NAME", "T").env("GIT_AUTHOR_EMAIL", "t@example.com").env("GIT_COMMITTER_NAME", "T").env("GIT_COMMITTER_EMAIL", "t@example.com").output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(())
}

/// `files` committed in the repository at `dir` (made the first time), tagged `tag`.
fn commit(dir: &Path, files: &[(&str, &str)], tag: Option<&str>) -> anyhow::Result<()> {
    for (path, text) in files {
        let p = dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap_or(dir))?;
        std::fs::write(&p, text)?;
    }
    if !dir.join(".git").exists() {
        git(dir, &["init", "-q", "-b", "main"])?;
    }
    git(dir, &["add", "."])?;
    git(dir, &["commit", "-qm", "change"])?;
    if let Some(t) = tag {
        git(dir, &["tag", "-f", t])?;
    }
    Ok(())
}

fn get(port: u16, admin: &str, path: &str) -> anyhow::Result<Value> {
    let (_, body) = http(port, "GET", path, admin, None)?;
    Ok(serde_json::from_str(&body).unwrap_or_default())
}

fn docker_works() -> bool {
    Command::new("docker").args(["info", "--format", "{{.ServerVersion}}"]).output().is_ok_and(|o| o.status.success())
}

const JS: &[(&str, &str)] = &[
    (
        "action.yml",
        "name: js kind\ninputs:\n  who:\n    description: whom to greet\n    default: ${{ github.repository }}\noutputs:\n  greeting:\n    description: the greeting\nruns:\n  using: node24\n  pre: pre.js\n  main: main.js\n  post: post.js\n",
    ),
    ("pre.js", "const fs = require('fs');\nfs.appendFileSync(process.env.GITHUB_STATE, 'from_pre=pre-was-here\\n');\nconsole.log(`js pre: who=${process.env.INPUT_WHO}`);\n"),
    (
        "main.js",
        "const fs = require('fs');\nconsole.log(`js main: who=${process.env.INPUT_WHO} state=${process.env.STATE_from_pre}`);\nfs.appendFileSync(process.env.GITHUB_OUTPUT, `greeting=hello ${process.env.INPUT_WHO}\\n`);\nfs.appendFileSync(process.env.GITHUB_STATE, 'from_main=main-was-here\\n');\n",
    ),
    ("post.js", "console.log(`js post: ${process.env.STATE_from_pre} ${process.env.STATE_from_main}`);\n"),
];

const DOCKER: &[(&str, &str)] = &[
    (
        "action.yml",
        "name: docker kind\ninputs:\n  who:\n    description: whom\n    default: docker\n  out:\n    description: a file to write\n    required: true\noutputs:\n  said:\n    description: what it said\nruns:\n  using: docker\n  image: Dockerfile\n  pre-entrypoint: /pre.sh\n  entrypoint: /main.sh\n  post-entrypoint: /post.sh\n  args:\n    - ${{ inputs.who }}\n  env:\n    OUT_FILE: ${{ inputs.out }}\n",
    ),
    ("Dockerfile", "FROM alpine:3.20\nCOPY pre.sh main.sh post.sh /\nRUN chmod +x /pre.sh /main.sh /post.sh\n"),
    ("pre.sh", "#!/bin/sh\necho \"docker pre: out=$OUT_FILE\"\necho 'docker_pre=pre-ran' >> \"$GITHUB_STATE\"\n"),
    ("main.sh", "#!/bin/sh\necho \"docker main: hello $1 state=$STATE_docker_pre\"\necho \"said=hello $1\" >> \"$GITHUB_OUTPUT\"\necho \"$INPUT_WHO wrote this\" > \"$OUT_FILE\"\n"),
    ("post.sh", "#!/bin/sh\necho \"docker post: $STATE_docker_pre\"\n"),
];

/// An image made for GitHub's runner user (uid 1001), writing in the workspace it's given.
const AS_RUNNER: &[(&str, &str)] = &[
    ("action.yml", "name: as runner\nruns:\n  using: docker\n  image: Dockerfile\n"),
    ("Dockerfile", "FROM alpine:3.20\nRUN adduser -D -u 1001 runner\nUSER 1001\nENTRYPOINT [\"sh\", \"-c\", \"echo written > as-runner.txt\"]\n"),
];

/// Three composites, each calling the next by its remote name, so their pins are followed too.
const STACK: &[(&str, &str)] = &[
    (
        "outer/action.yml",
        "name: outer\ninputs:\n  word:\n    description: w\n    required: true\noutputs:\n  word:\n    description: the word, three levels down and back\n    value: ${{ steps.mid.outputs.word }}\nruns:\n  using: composite\n  steps:\n    - id: mid\n      uses: kinds/stack/middle@v1\n      with:\n        word: ${{ inputs.word }}-outer\n",
    ),
    (
        "middle/action.yml",
        "name: middle\ninputs:\n  word:\n    description: w\n    required: true\noutputs:\n  word:\n    description: w\n    value: ${{ steps.in.outputs.word }}\nruns:\n  using: composite\n  steps:\n    - id: in\n      uses: kinds/stack/inner@v1\n      with:\n        word: ${{ inputs.word }}-middle\n",
    ),
    (
        "inner/action.yml",
        "name: inner\ninputs:\n  word:\n    description: w\n    required: true\noutputs:\n  word:\n    description: w\n    value: ${{ steps.say.outputs.word }}\nruns:\n  using: composite\n  steps:\n    - id: say\n      shell: bash\n      run: echo \"word=${{ inputs.word }}-inner\" >> \"$GITHUB_OUTPUT\"\n",
    ),
];

const WORKFLOW: &str = r#"on: push
jobs:
  kinds:
    runs-on: local
    steps:
      - uses: kiln/checkout@v1
      - id: js
        uses: kinds/js@v1
      - id: dock
        uses: kinds/docker@v1
        with:
          who: whale
          out: from-docker.txt
      - uses: kinds/as-runner@v1
      - id: url
        uses: docker://alpine:3.20
        with:
          entrypoint: /bin/sh
          args: -c "echo url kind; echo said=from-url >> $GITHUB_OUTPUT"
      - id: stack
        uses: kinds/stack/outer@v1
        with:
          word: deep
      - id: local
        uses: ./.github/actions/local
        with:
          name: here
      - run: |
          echo "js=${{ steps.js.outputs.greeting }}"
          echo "docker=${{ steps.dock.outputs.said }} file=$(cat from-docker.txt)"
          echo "url=${{ steps.url.outputs.said }}"
          echo "stack=${{ steps.stack.outputs.word }}"
          echo "local=${{ steps.local.outputs.echoed }}"
          echo "as-runner=$(cat as-runner.txt) owner=$(stat -c %u as-runner.txt)"
"#;

const LOCAL: &str = "name: local\ninputs:\n  name:\n    description: n\n    default: nobody\noutputs:\n  echoed:\n    description: e\n    value: ${{ steps.e.outputs.echoed }}\nruns:\n  using: composite\n  steps:\n    - id: e\n      shell: bash\n      run: echo \"echoed=local ${{ inputs.name }}\" >> \"$GITHUB_OUTPUT\"\n";

/// A server with its own runner whose `kinds/*` actions come from `src`, polling `repo`, once it
/// has taken the repository as it is (the push after its first poll runs).
fn server_for(dir: &Path, src: &Path, repo: &Path) -> anyhow::Result<common::Server> {
    commit(repo, &[("README.md", "kinds\n")], None)?;
    let ci = format!("\n[[ci.actions.sources]]\nowners = [\"kinds\"]\nurl = \"file://{}/{{owner}}/{{repo}}\"\n", src.display());
    let extra = format!("[[repos]]\nname = \"acme/kinds\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
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

/// The newest run past `after` runs, once it finished: the run and its legs (each with its steps
/// and annotations).
fn finished(s: &common::Server, after: usize) -> anyhow::Result<(Value, Vec<Value>)> {
    let deadline = Instant::now() + Duration::from_secs(600);
    let get = |path: &str| get(s.port, &s.admin, path).map_err(|e| anyhow::anyhow!("GET {path}: {e}\n{}", s.log()));
    loop {
        let runs = get("/api/ci/runs?repo=acme/kinds&limit=20")?;
        let list = runs.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        if list.len() > after
            && let Some(r) = list.first().filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null()))
        {
            let id = r.get("id").and_then(Value::as_str).unwrap_or_default();
            let detail = get(&format!("/api/ci/runs/{id}"))?;
            let mut legs = Vec::new();
            for l in detail.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
                let leg = l.get("id").and_then(Value::as_str).unwrap_or_default();
                legs.push(get(&format!("/api/ci/legs/{leg}"))?);
            }
            return Ok((r.clone(), legs));
        }
        anyhow::ensure!(Instant::now() < deadline, "no finished run past {after}: {runs}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// A leg of `legs` by its job's key: its conclusion and its annotations' messages.
fn leg_of(legs: &[Value], key: &str) -> (String, String) {
    let l = legs.iter().find(|l| l.pointer("/leg/key").and_then(Value::as_str) == Some(key)).cloned().unwrap_or_default();
    let notes: Vec<String> = l.get("annotations").and_then(Value::as_array).into_iter().flatten().filter_map(|a| a.get("message").and_then(Value::as_str).map(str::to_owned)).collect();
    (l.pointer("/leg/conclusion").and_then(Value::as_str).unwrap_or_default().to_owned(), notes.join("\n"))
}

const HELLO: &[(&str, &str)] = &[("action.yml", "name: hello\nruns:\n  using: composite\n  steps:\n    - shell: bash\n      run: echo \"hello from kinds/hello\"\n")];

const TWO_JOBS: &str = "on: push\njobs:\n  mirrored:\n    runs-on: local\n    steps:\n      - uses: kinds/hello@v1\n  gone:\n    runs-on: local\n    steps:\n      - run: echo never\n      - uses: kinds/gone@v1\n";

/// AC03: with the action source unreachable, a pinned action still runs, from the mirror. One
/// never mirrored fails with the message, and only its job, as GitHub fails a job whose action
/// it can't resolve at "Set up job": the run's other jobs go on.
#[test]
fn w07_ac03_mirror_offline() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("src");
    commit(&src.join("kinds/hello"), HELLO, Some("v1"))?;
    let repo = dir.path().join("repo");
    let s = server_for(dir.path(), &src, &repo)?;
    commit(&repo, &[(".github/workflows/two.yml", TWO_JOBS)], None)?;
    let (run, legs) = finished(&s, 0)?;
    assert_eq!(run.get("conclusion").and_then(Value::as_str), Some("failure"), "the run starts, and fails by its one job: {run}");
    let (ok, _) = leg_of(&legs, "mirrored");
    assert_eq!(ok, "success", "{legs:?}");
    let (gone, why) = leg_of(&legs, "gone");
    assert_eq!(gone, "failure", "{legs:?}");
    assert!(why.contains("Unable to resolve action `kinds/gone@v1`") && why.contains("action source unreachable and not mirrored"), "{why}");
    // The source goes away; the next run's pin comes from the mirror.
    std::fs::rename(&src, dir.path().join("src-gone"))?;
    commit(&repo, &[("README.md", "again\n")], None)?;
    let (_, legs) = finished(&s, 1)?;
    let (ok, notes) = leg_of(&legs, "mirrored");
    assert_eq!(ok, "success", "a pinned action runs from the mirror: {notes}\n{}", s.log());
    let (gone, why) = leg_of(&legs, "gone");
    assert!(gone == "failure" && why.contains("action source unreachable and not mirrored"), "{gone}: {why}");
    Ok(())
}

/// The legs of the run's attempt `attempt` (a rerun is the run's next attempt, its legs new),
/// once every one has finished: (job, log of its steps).
fn attempt_logs(s: &common::Server, run: &str, attempt: i64) -> anyhow::Result<Vec<(String, String)>> {
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let detail = get(s.port, &s.admin, &format!("/api/ci/runs/{run}?attempt={attempt}"))?;
        let legs = detail.get("legs").and_then(Value::as_array).cloned().unwrap_or_default();
        let done = legs.len() == 2 && legs.iter().all(|l| l.get("conclusion").is_some_and(|c| !c.is_null()));
        if done {
            let mut out = Vec::new();
            for l in &legs {
                let id = l.get("id").and_then(Value::as_str).unwrap_or_default();
                let mut log = String::new();
                for step in 0..3 {
                    let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{id}/logs?step={step}"), &s.admin, None)?;
                    log.push_str(&text);
                }
                out.push((l.get("key").and_then(Value::as_str).unwrap_or_default().to_owned(), log));
            }
            return Ok(out);
        }
        let state: Vec<String> = legs.iter().map(|l| format!("{} attempt {} {} {}", l.get("key").and_then(Value::as_str).unwrap_or_default(), l.get("attempt").and_then(Value::as_i64).unwrap_or(0), l.get("status").and_then(Value::as_str).unwrap_or_default(), l.get("conclusion").and_then(Value::as_str).unwrap_or("-"))).collect();
        anyhow::ensure!(Instant::now() < deadline, "attempt {attempt} didn't finish: {state:?}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn says(legs: &[(String, String)], job: &str) -> String {
    let log = legs.iter().find(|(k, _)| k == job).map(|(_, l)| l.as_str()).unwrap_or_default();
    ["one", "two"].iter().find(|w| log.contains(&format!("kinds/hello says {w}"))).map_or_else(|| format!("nothing in: {log}"), |w| (*w).to_owned())
}

/// AC02: a run resolves `kinds/hello@v1` once. Moving the tag while the run is underway doesn't
/// change what its later legs get, nor what a rerun gets; "rerun with fresh actions" pins it
/// again and gets the commit the tag names now.
#[test]
fn w07_ac02_pins() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("src");
    let hello = |word: &str| format!("name: hello\nruns:\n  using: composite\n  steps:\n    - shell: bash\n      run: echo \"kinds/hello says {word}\"\n");
    commit(&src.join("kinds/hello"), &[("action.yml", &hello("one"))], Some("v1"))?;
    let repo = dir.path().join("repo");
    let s = server_for(dir.path(), &src, &repo)?;
    // The first job waits, its action already run, until the test has moved the tag.
    let go = dir.path().join("go");
    let wf = format!(
        "on: push\njobs:\n  first:\n    runs-on: local\n    steps:\n      - uses: kinds/hello@v1\n      - run: while [ ! -e '{}' ]; do sleep 0.2; done\n  second:\n    needs: first\n    runs-on: local\n    steps:\n      - uses: kinds/hello@v1\n",
        go.display()
    );
    commit(&repo, &[(".github/workflows/pins.yml", &wf)], None)?;
    let deadline = Instant::now() + Duration::from_secs(300);
    let run = loop {
        let runs = get(s.port, &s.admin, "/api/ci/runs?repo=acme/kinds&limit=5")?;
        if let Some(id) = runs.pointer("/runs/0/id").and_then(Value::as_str) {
            let detail = get(s.port, &s.admin, &format!("/api/ci/runs/{id}"))?;
            let first_running = detail.get("legs").and_then(Value::as_array).into_iter().flatten().any(|l| l.get("key").and_then(Value::as_str) == Some("first") && l.get("status").and_then(Value::as_str) == Some("running"));
            if first_running {
                break id.to_owned();
            }
        }
        anyhow::ensure!(Instant::now() < deadline, "the run never got going: {runs}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(300));
    };
    // The tag moves on while the run is underway.
    commit(&src.join("kinds/hello"), &[("action.yml", &hello("two"))], Some("v1"))?;
    std::fs::write(&go, "")?;
    let legs = attempt_logs(&s, &run, 1)?;
    assert_eq!((says(&legs, "first").as_str(), says(&legs, "second").as_str()), ("one", "one"), "the run's legs keep its pin");
    // A rerun keeps the run's pins.
    let (st, body) = http(s.port, "POST", &format!("/api/ci/runs/{run}/rerun"), &s.admin, Some(&serde_json::json!({ "mode": "all" })))?;
    anyhow::ensure!(st == 201, "rerun: {st} {body}");
    let legs = attempt_logs(&s, &run, 2)?;
    assert_eq!((says(&legs, "first").as_str(), says(&legs, "second").as_str()), ("one", "one"), "a rerun keeps the run's pins");
    // Rerun with fresh actions: pinned again, now.
    let (st, body) = http(s.port, "POST", &format!("/api/ci/runs/{run}/rerun"), &s.admin, Some(&serde_json::json!({ "mode": "all", "fresh_actions": true })))?;
    anyhow::ensure!(st == 201, "rerun with fresh actions: {st} {body}");
    let legs = attempt_logs(&s, &run, 3)?;
    assert_eq!((says(&legs, "first").as_str(), says(&legs, "second").as_str()), ("two", "two"), "fresh actions get what the tag names now");
    Ok(())
}

/// A kiln action declaring the secret it needs (W07-R11, W09-R07).
const DEPLOY: &[(&str, &str)] = &[(
    "action.yml",
    "name: deploy\nsecrets:\n  DEPLOY_KEY: { required: true, description: \"Key for the deploy host\" }\nruns:\n  using: composite\n  steps:\n    - shell: bash\n      run: echo \"deploying with ${{ secrets.DEPLOY_KEY }}\"\n",
)];

/// AC06: an action declaring a required secret fails the run as it starts (`KCI4040`, saying
/// where it looked and how to add it) while the secret isn't set, and runs once it is, the
/// secret there inside it and masked.
#[test]
fn w07_ac06_declared_secrets() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("src");
    commit(&src.join("kinds/deploy"), DEPLOY, Some("v1"))?;
    let repo = dir.path().join("repo");
    let s = server_for(dir.path(), &src, &repo)?;
    commit(&repo, &[(".github/workflows/deploy.yml", "on: push\njobs:\n  ship:\n    runs-on: local\n    steps:\n      - uses: kinds/deploy@v1\n")], None)?;
    let (run, _) = finished(&s, 0)?;
    assert_eq!(run.get("conclusion").and_then(Value::as_str), Some("startup_failure"), "{run}");
    let why = run.get("error").and_then(Value::as_str).unwrap_or_default();
    assert!(why.contains("KCI4040") && why.contains("`kinds/deploy@v1` in job `ship` needs the secret DEPLOY_KEY") && why.contains("kiln secrets set DEPLOY_KEY --scope repo:acme/kinds"), "{why}");
    let (st, body) = http(s.port, "PUT", "/api/ci/secrets/DEPLOY_KEY?scope=repo:acme/kinds", &s.admin, Some(&serde_json::json!({ "value": "hunter2-deploy-key" })))?;
    anyhow::ensure!(st < 300, "set the secret: {st} {body}");
    commit(&repo, &[("README.md", "again\n")], None)?;
    let (run, legs) = finished(&s, 1)?;
    assert_eq!(run.get("conclusion").and_then(Value::as_str), Some("success"), "{run}");
    let leg = legs.first().and_then(|l| l.pointer("/leg/id")).and_then(Value::as_str).unwrap_or_default();
    let (_, log) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step=0"), &s.admin, None)?;
    assert!(log.contains("deploying with ***") && !log.contains("hunter2-deploy-key"), "the action has the secret, masked: {log}");
    Ok(())
}

#[test]
fn w07_ac04_kinds() -> anyhow::Result<()> {
    if !docker_works() {
        eprintln!("w07_ac04_kinds: skipped, no Docker");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("src");
    commit(&src.join("kinds/js"), JS, Some("v1"))?;
    commit(&src.join("kinds/docker"), DOCKER, Some("v1"))?;
    commit(&src.join("kinds/stack"), STACK, Some("v1"))?;
    commit(&src.join("kinds/as-runner"), AS_RUNNER, Some("v1"))?;
    // The machine's Node 24, when it has one, so the runner needn't fetch it.
    if let Some(node) = std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("node")).find(|n| n.is_file()))
        && Command::new(&node).arg("--version").output().is_ok_and(|v| String::from_utf8_lossy(&v.stdout).starts_with("v24."))
    {
        let bin = dir.path().join("work/_externals/node24/bin");
        std::fs::create_dir_all(&bin)?;
        std::os::unix::fs::symlink(&node, bin.join("node"))?;
    }
    let repo = dir.path().join("repo");
    commit(&repo, &[("README.md", "kinds\n")], None)?;
    let ci = format!("\n[[ci.actions.sources]]\nowners = [\"kinds\"]\nurl = \"file://{}/{{owner}}/{{repo}}\"\n", src.display());
    let extra = format!("[[repos]]\nname = \"acme/kinds\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    let s = start(dir.path(), &Options { agent: true, ci: &ci, extra: &extra })?;
    // The first poll takes the repository as it is; the push after it runs.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        if body.contains("acme/kinds") && !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    commit(&repo, &[(".github/workflows/kinds.yml", WORKFLOW), (".github/actions/local/action.yml", LOCAL)], None)?;
    let deadline = Instant::now() + Duration::from_secs(600);
    let (run, leg) = loop {
        let runs = get(s.port, &s.admin, "/api/ci/runs?repo=acme/kinds&limit=5")?;
        if let Some(r) = runs.get("runs").and_then(Value::as_array).and_then(|r| r.first()).filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())) {
            let id = r.get("id").and_then(Value::as_str).unwrap_or_default();
            let detail = get(s.port, &s.admin, &format!("/api/ci/runs/{id}"))?;
            let leg = detail.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
            break (r.clone(), get(s.port, &s.admin, &format!("/api/ci/legs/{leg}"))?);
        }
        anyhow::ensure!(Instant::now() < deadline, "the run didn't finish: {runs}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let leg_id = leg.pointer("/leg/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    // Each step's name and log, in the order they ran.
    let mut steps: Vec<Value> = leg.get("steps").and_then(Value::as_array).cloned().unwrap_or_default();
    steps.sort_by_key(|x| x.get("seq").and_then(Value::as_i64).unwrap_or(i64::MAX));
    let mut ran = Vec::new();
    let mut all = String::new();
    for x in &steps {
        let idx = x.get("idx").and_then(Value::as_i64).unwrap_or_default();
        let (_, log) = http(s.port, "GET", &format!("/api/ci/legs/{leg_id}/logs?step={idx}"), &s.admin, None)?;
        let name = x.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
        all.push_str(&format!("--- {name} ({})\n{log}\n", x.get("conclusion").and_then(Value::as_str).unwrap_or_default()));
        ran.push((name, log));
    }
    assert_eq!(run.get("conclusion").and_then(Value::as_str), Some("success"), "{run}\n{all}\n{}", s.log());
    let at = |what: &str| ran.iter().position(|(_, l)| l.contains(what)).ok_or_else(|| anyhow::anyhow!("{what:?} in no step:\n{all}"));
    // Inputs: defaults evaluated, `with:` taken, `runs.args` and `runs.env` with the action's own.
    // State: a pre step's for its main and post steps, a main step's for its post step.
    for want in [
        "js pre: who=acme/kinds",
        "js main: who=acme/kinds state=pre-was-here",
        "js post: pre-was-here main-was-here",
        "docker pre: out=from-docker.txt",
        "docker main: hello whale state=pre-ran",
        "docker post: pre-ran",
        "url kind",
    ] {
        at(want)?;
    }
    // Outputs, of every kind, and what the Docker action wrote in the workspace.
    for want in ["js=hello acme/kinds", "docker=hello whale file=whale wrote this", "url=from-url", "stack=deep-outer-middle-inner", "local=local here"] {
        at(want)?;
    }
    // An image made for GitHub's runner user writes in the workspace as this runner's user.
    let uid = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(dir.path())?);
    at(&format!("as-runner=written owner={uid}"))?;
    // Order, as GitHub runs them: the pre steps before the job's own steps, in step order; the
    // post steps after them, in reverse.
    let [js_pre, docker_pre, js_main, docker_main, url, outputs, docker_post, js_post] = ["js pre", "docker pre", "js main", "docker main", "url kind", "js=hello", "docker post", "js post"].map(at);
    let (js_pre, docker_pre, js_main, docker_main, url, outputs, docker_post, js_post) = (js_pre?, docker_pre?, js_main?, docker_main?, url?, outputs?, docker_post?, js_post?);
    assert!(js_pre < docker_pre && docker_pre < js_main, "pre steps first, in step order:\n{all}");
    assert!(docker_main < url && url < outputs, "the job's steps in order:\n{all}");
    assert!(outputs < docker_post && docker_post < js_post, "post steps last, in reverse:\n{all}");
    let named = |i: usize, prefix: &str| ran.get(i).is_some_and(|(n, _)| n.starts_with(prefix));
    assert!(named(js_pre, "Pre ") && named(docker_post, "Post "), "GitHub's names: {:?}", ran.iter().map(|(n, _)| n).collect::<Vec<_>>());
    Ok(())
}
