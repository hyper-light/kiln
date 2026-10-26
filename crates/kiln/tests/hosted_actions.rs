//! W14-AC05 and W07-R09 (docs/ci.md): GitHub's own actions against a native repository, end to
//! end: `actions/checkout` at a diff's version with its submodule, `actions/download-artifact`
//! across runs, and a comment action on the pull request. They come from github.com (pinned as the
//! run starts and mirrored), so the test runs only when github.com answers.

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

/// W13-R04: a `docker` leg in kiln's runner image has its hosted runner's user: `runner`, by
/// `USER` and `LOGNAME` too, as a hosted runner's steps have them. ORT's action makes its
/// container's paths with `${PROJECT_PATH/$USER/ort}`: without `USER` they stayed the host's, and
/// the corpus's elixir found "path does not exist".
#[test]
fn w13_r04_a_hosted_legs_steps_say_their_user() -> anyhow::Result<()> {
    let Some((s, run, log)) = in_image("kiln/runner-ubuntu-24.04", "echo \"user=$USER logname=$LOGNAME home=$HOME ort=${GITHUB_WORKSPACE/$USER/ort}\"")? else { return Ok(()) };
    assert_eq!(run.get("conclusion"), Some(&json!("success")), "{run}\n{log}\n{}", s.log());
    assert!(log.contains("user=runner logname=runner home=/home/runner ort=/home/ort/work/app/app"), "{log}");
    Ok(())
}

/// W13-R04, the plan's step 7: kiln's opt-in Android image (the user accepted the SDK's licence,
/// 2026-10-23) has NDK 27 where GitHub's images have it, its LLVM toolchain the one uv's Android
/// job builds with, and `ANDROID_NDK_*` naming it as GitHub's images do. Built here once
/// (`kiln workflow run` of a job routed to it builds it), it's checked by a leg in it.
#[test]
fn w13_r04_the_android_image_has_ndk_27s_toolchain() -> anyhow::Result<()> {
    let ndk = "/usr/local/lib/android/sdk/ndk/27.3.13750724";
    let step = "echo \"home=$ANDROID_HOME ndk=$ANDROID_NDK ndk_home=$ANDROID_NDK_HOME ndk_root=$ANDROID_NDK_ROOT latest=$ANDROID_NDK_LATEST_HOME\" && ls -d \"$ANDROID_HOME\"/ndk/*/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android24-clang && sdkmanager --version && adb version | head -1";
    let Some((s, run, log)) = in_image("kiln/runner-ubuntu-24.04-android", step)? else { return Ok(()) };
    assert_eq!(run.get("conclusion"), Some(&json!("success")), "{run}\n{log}\n{}", s.log());
    assert!(log.contains(&format!("home=/usr/local/lib/android/sdk ndk={ndk} ndk_home={ndk} ndk_root={ndk} latest={ndk}")), "{log}");
    assert!(log.contains(&format!("{ndk}/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android24-clang")), "{log}");
    Ok(())
}

/// A push of a workflow whose one job runs `step` in `image` (a `docker` leg) on a server of its
/// own: the server, the run and its log; none when there's no Docker or the image isn't built.
fn in_image(image: &str, step: &str) -> anyhow::Result<Option<(Server, Value, String)>> {
    if !docker_works() {
        eprintln!("skipped: no Docker here");
        return Ok(None);
    }
    let images = Command::new("docker").args(["images", "--format", "{{.Repository}}"]).output()?;
    if !String::from_utf8_lossy(&images.stdout).lines().any(|l| l == image) {
        eprintln!("skipped: {image} isn't built on this machine");
        return Ok(None);
    }
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    // On this machine's network: the server listens on its loopback only.
    let rule = format!("[[ci.routing]]\nmatch = {{ labels = [\"hosted\"] }}\nexecutor = [\"docker\"]\nimage = \"{image}\"\n");
    let s = common::start_env(dir.path(), &Options { agent: true, ci: "", extra: &rule }, false, &[("KILN_CONTAINER_NETWORK", "host")])?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&home, dir.path(), &["clone", "-q", &url, "work"])?;
    let work = dir.path().join("work");
    git(&home, &work, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "README.md"])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    let workflow = format!("on: push\njobs:\n  step:\n    runs-on: hosted\n    steps:\n      - run: {}\n", serde_json::to_string(step)?);
    std::fs::write(work.join(".github/workflows/step.yml"), workflow)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "step"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;
    let (run, log) = finished(&s, "push", 900)?;
    Ok(Some((s, run, log)))
}

/// Whether github.com answers here.
fn github() -> bool {
    Command::new("curl").args(["-fsSI", "--max-time", "10", "https://github.com/actions/checkout"]).output().is_ok_and(|o| o.status.success())
}

/// The newest finished run for `event`: it and its first leg's steps' logs.
fn finished(s: &Server, event: &str, secs: u64) -> anyhow::Result<(Value, String)> {
    let mut run = Value::Null;
    until(s, secs, &format!("a {event} run"), || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=10", &s.admin, None)?;
        let runs = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        match runs.into_iter().find(|r| r.get("event").and_then(Value::as_str) == Some(event)) {
            Some(r) if r.get("finished_at").is_some_and(|f| !f.is_null()) => {
                run = r;
                Ok(None)
            }
            _ => Ok(Some(body)),
        }
    })?;
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
    let mut log = String::new();
    for step in serde_json::from_str::<Value>(&about)?.get("steps").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.get("idx").and_then(Value::as_u64)) {
        let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
        log.push_str(&text);
    }
    Ok((run, log))
}

const PRODUCE: &str = r#"on:
  push: { branches: [main] }
jobs:
  produce:
    runs-on: local
    steps:
      - run: echo "made in run one" > made.txt
      - uses: actions/upload-artifact@v4
        with: { name: made, path: made.txt }
"#;

const CONSUME: &str = r#"on: pull_request
permissions:
  contents: read
  actions: read
  pull-requests: write
  issues: write
jobs:
  consume:
    runs-on: local
    steps:
      - uses: actions/checkout@v4
        with:
          ref: ${{ github.event.pull_request.head.sha }}
          submodules: true
      - run: |
          test "$(cat change.txt)" = changed && echo "at the diff's version"
          test "$(cat sub/lib.txt)" = "the library" && echo "with its submodule"
      - uses: actions/download-artifact@v4
        with:
          name: made
          run-id: ${{ vars.PRODUCER_RUN }}
          github-token: ${{ github.token }}
      - run: test "$(cat made.txt)" = "made in run one" && echo "from the other run"
      - uses: peter-evans/create-or-update-comment@v4
        with:
          issue-number: ${{ github.event.pull_request.number }}
          body: from an action
"#;

/// W14-AC05's parts left (with W07-R09): a pull request's job checks the diff's version out with
/// its submodule, downloads an artifact another run made, and comments on the pull request, all
/// with GitHub's own actions against kiln.
#[test]
fn w14_ac05_github_actions_on_a_native_repository() -> anyhow::Result<()> {
    if !github() {
        eprintln!("skipped: github.com doesn't answer here, and the actions come from it");
        return Ok(());
    }
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let s = start(dir.path(), &Options { agent: true, ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&home, dir.path(), &["clone", "-q", &url, "work"])?;
    let work = dir.path().join("work");
    // The trunk first, so it's the trunk.
    git(&home, &work, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "README.md"])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    // The library the submodule names: a branch of the repository itself.
    git(&home, &work, &["checkout", "-q", "--orphan", "lib"])?;
    git(&home, &work, &["rm", "-rfq", "--cached", "."])?;
    std::fs::remove_file(work.join("README.md"))?;
    std::fs::write(work.join("lib.txt"), "the library")?;
    git(&home, &work, &["add", "lib.txt"])?;
    git(&home, &work, &["commit", "-qm", "the library"])?;
    git(&home, &work, &["push", "-q", "origin", "lib"])?;
    git(&home, &work, &["checkout", "-q", "main"])?;
    git(&home, &work, &["-c", "protocol.file.allow=always", "submodule", "add", "-q", "-b", "lib", "../app.git", "sub"])?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(work.join(".github/workflows/produce.yml"), PRODUCE)?;
    std::fs::write(work.join(".github/workflows/consume.yml"), CONSUME)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "the workflows and the library"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;
    // The producer's run, and its number for the consumer.
    let (produced, log) = finished(&s, "push", 600)?;
    assert_eq!(produced.get("conclusion"), Some(&json!("success")), "{produced}\n{log}\n{}", s.log());
    let number = produced.get("github_run_id").cloned().unwrap_or_default();
    let (st, body) = http(s.port, "PUT", "/api/ci/variables/PRODUCER_RUN?scope=repo:acme/app", &s.admin, Some(&json!({ "value": number.to_string() })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    // A diff, and its pull request's run.
    std::fs::write(work.join("change.txt"), "changed")?;
    git(&home, &work, &["add", "change.txt"])?;
    git(&home, &work, &["commit", "-qm", "a change\n\nChange-Id: I2222222222222222222222222222222222222222"])?;
    git(&home, &work, &["push", "-q", "origin", "HEAD:refs/for/main"])?;
    let (consumed, log) = finished(&s, "pull_request", 900)?;
    assert_eq!(consumed.get("conclusion"), Some(&json!("success")), "{consumed}\n{log}\n{}", s.log());
    for said in ["at the diff's version", "with its submodule", "from the other run"] {
        assert!(log.contains(said), "{said}: {log}");
    }
    // The comment is the diff's, as the job's.
    let (_, threads) = http(s.port, "GET", "/api/diffs/D1/comments", &s.admin, None)?;
    assert!(threads.contains("from an action"), "{threads}");
    Ok(())
}

/// `actions/checkout` v7 (as the corpus pins it, by commit): its credentials persisted in a
/// configuration file of their own under `RUNNER_TEMP`, which a later `git fetch` in the job uses:
/// on the host, and (`CHECKOUT_V7_DOCKER`, where Docker is) in a job container and in the `docker`
/// executor. In the job container, root over a workspace this runner's user owns, git refuses the
/// repository until it's a safe directory, as on GitHub (checkout adds it only to a global
/// configuration of its own while it runs).
const CHECKOUT_V7: &str = r#"on: push
jobs:
  host:
    runs-on: local
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1
      - run: git fetch --tags && echo "fetched again"
"#;

/// [`CHECKOUT_V7`]'s jobs that need Docker: a job container, and the `docker` executor.
const CHECKOUT_V7_DOCKER: &str = r#"  container:
    runs-on: local
    container: docker.io/library/node:24-bookworm
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1
      - run: |
          git config --global --add safe.directory "$GITHUB_WORKSPACE"
          git fetch --tags && echo "fetched again"
  executor:
    runs-on: v7exec
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1
      - run: git fetch --tags && echo "fetched again"
"#;

fn docker_works() -> bool {
    Command::new("docker").args(["info", "--format", "{{.ServerVersion}}"]).output().is_ok_and(|o| o.status.success())
}

#[test]
fn w07_r09_checkout_v7_persists_its_credentials() -> anyhow::Result<()> {
    if !github() {
        eprintln!("skipped: github.com doesn't answer here, and the action comes from it");
        return Ok(());
    }
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    // The job container on this machine's network: the server listens on its loopback only. The
    // `docker` executor's image has Node and git.
    let rule = "[[ci.routing]]\nmatch = { labels = [\"v7exec\"] }\nexecutor = [\"docker\"]\nimage = \"docker.io/library/node:24-bookworm\"\n";
    let s = common::start_env(dir.path(), &Options { agent: true, ci: "", extra: rule }, false, &[("KILN_CONTAINER_NETWORK", "host")])?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&home, dir.path(), &["clone", "-q", &url, "work"])?;
    let work = dir.path().join("work");
    git(&home, &work, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
    std::fs::write(work.join("README.md"), "app\n")?;
    git(&home, &work, &["add", "README.md"])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    // Where no Docker answers (a CI pod), the host's job alone: the others would wait for it, and
    // the run never end.
    let docker = docker_works();
    if !docker {
        eprintln!("no Docker here: the host's job runs, not the job container's or the docker executor's");
    }
    std::fs::write(work.join(".github/workflows/checkout.yml"), if docker { format!("{CHECKOUT_V7}{CHECKOUT_V7_DOCKER}") } else { CHECKOUT_V7.to_owned() })?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "checkout v7"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;
    let mut run = Value::Null;
    until(&s, 900, "the run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=5", &s.admin, None)?;
        run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0").cloned().unwrap_or_default();
        Ok(run.get("finished_at").is_none_or(Value::is_null).then_some(body))
    })?;
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    for l in serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
        let leg = l.get("id").and_then(Value::as_str).unwrap_or_default();
        let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
        let about: Value = serde_json::from_str(&about)?;
        let mut log = String::new();
        for x in about.get("steps").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.get("idx").and_then(Value::as_u64)) {
            log.push_str(&http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={x}"), &s.admin, None)?.1);
        }
        let key = about.pointer("/leg/key").and_then(Value::as_str).unwrap_or_default().to_owned();
        assert_eq!(about.pointer("/leg/conclusion"), Some(&json!("success")), "{key}: {about}\n{log}\n{}", s.log());
        assert!(log.contains("fetched again"), "{key}: {log}");
    }
    Ok(())
}
