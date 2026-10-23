//! W08-R11 and W08-AC03 (docs/ci.md): BuildKit's caches through kiln, as `docker/build-push-action`
//! uses them: `type=gha` through kiln's cache service, and `type=registry` in kiln's registry. A
//! build exports its layers' cache (`mode=max`); a second run's build, on a builder of its own
//! (nothing in the builder's own cache), imports it and takes every layer from it. Skipped without
//! Docker, buildx and buildx's BuildKit image on this machine (the tests pull nothing).

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, http, start};
use serde_json::{Value, json};

fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let o = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    Ok(())
}

/// Docker with buildx, and the BuildKit image buildx's `docker-container` driver runs.
fn buildkit_here() -> bool {
    let ok = |args: &[&str]| Command::new("docker").args(args).output().is_ok_and(|o| o.status.success());
    ok(&["buildx", "version"]) && ok(&["image", "inspect", "moby/buildkit:buildx-stable-1"])
}

/// Two layers from `scratch` (nothing to pull), each built on a builder of the run's own whose
/// network is the host's (the cache service is on this machine's loopback), its cache to and from
/// kiln's: `cache` is buildx's `--cache-to`/`--cache-from` type and options (`$ref`, kiln's
/// registry's name for the cache, set first).
fn workflow(cache: &str) -> String {
    format!(
        r#"on: push
jobs:
  image:
    runs-on: local
    checkout: false
    permissions: {{ packages: write }}
    steps:
      - run: |
          mkdir -p ctx && cd ctx
          printf 'FROM scratch\nCOPY a.txt /a.txt\nCOPY b.txt /b.txt\n' > Dockerfile
          echo one > a.txt
          echo two > b.txt
          reg="${{GITHUB_SERVER_URL#*://}}"; reg="${{reg%%/*}}"
          ref="$reg/acme/image:buildcache"
          export DOCKER_CONFIG="$RUNNER_TEMP/docker"
          echo "$KILN_TOKEN" | docker login "$reg" -u kiln --password-stdin > /dev/null
          b="kiln-w08-$KILN_RUN"
          docker buildx create --name "$b" --driver docker-container --driver-opt network=host --bootstrap > /dev/null
          trap 'docker buildx rm -f "$b" > /dev/null 2>&1' EXIT
          docker buildx build --builder "$b" --progress plain --output type=cacheonly \
            --cache-from "{cache}" --cache-to "{cache},mode=max" . 2>&1
"#
    )
}

/// The workflow run twice on a server of its own, the second after the first: each run and its
/// log.
fn twice(workflow: &str) -> anyhow::Result<[(Value, String); 2]> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/image", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("work"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/image.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "image\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &url, "main"])?;
    let polled = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        if body.contains("acme/image") && !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(Instant::now() < polled, "the repository was never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    // The finished runs of the workflow, newest first, once there are `n`.
    let runs = |n: usize| -> anyhow::Result<Vec<Value>> {
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/image&limit=10", &s.admin, None)?;
            let list: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
            let done: Vec<Value> =
                list.into_iter().filter(|r| r.get("workflow").and_then(Value::as_str).is_some_and(|w| w.ends_with("image.yaml")) && r.get("finished_at").is_some_and(|f| !f.is_null())).collect();
            if done.len() >= n {
                return Ok(done);
            }
            anyhow::ensure!(Instant::now() < deadline, "{n} runs didn't finish: {body}\n{}", s.log());
            std::thread::sleep(Duration::from_millis(500));
        }
    };
    let log_of = |run: &Value| -> anyhow::Result<String> {
        let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
        let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let (_, log) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step=0"), &s.admin, None)?;
        Ok(log)
    };
    std::fs::write(work.join(".kiln/image.yaml"), workflow)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "an image"])?;
    git(&home, &work, &["push", "-q", &url, "main"])?;
    let first = runs(1)?.first().cloned().unwrap_or_default();
    let first_log = log_of(&first)?;
    anyhow::ensure!(first.get("conclusion").and_then(Value::as_str) == Some("success"), "the first run: {first_log}\n{}", s.log());
    std::fs::write(work.join("README.md"), "image, again\n")?;
    git(&home, &work, &["commit", "-qam", "again"])?;
    git(&home, &work, &["push", "-q", &url, "main"])?;
    let second = runs(2)?.first().cloned().unwrap_or_default();
    let second_log = log_of(&second)?;
    anyhow::ensure!(second.get("conclusion").and_then(Value::as_str) == Some("success"), "the second run: {second_log}\n{}", s.log());
    Ok([(first, first_log), (second, second_log)])
}

/// Whether a build's log says each of `layers` came from the cache: `--progress plain` says a
/// step as `#N [1/2] COPY …`, and later `#N CACHED`.
fn all_cached(log: &str, layers: &[&str]) -> bool {
    layers.iter().all(|layer| {
        let id = log.lines().find(|l| l.contains(layer)).and_then(|l| l.split_whitespace().find(|w| w.starts_with('#')).map(str::to_owned));
        id.is_some_and(|id| log.lines().any(|l| l.split_whitespace().any(|w| w == id) && l.contains("CACHED")))
    })
}

const LAYERS: [&str; 2] = ["COPY a.txt /a.txt", "COPY b.txt /b.txt"];

#[test]
fn w08_ac03_buildkit_gha_cache() -> anyhow::Result<()> {
    if !buildkit_here() {
        eprintln!("w08_ac03_buildkit_gha_cache: skipped, no Docker with buildx and moby/buildkit:buildx-stable-1 here");
        return Ok(());
    }
    let [(_, first), (_, second)] = twice(&workflow("type=gha,url=$ACTIONS_CACHE_URL,token=$ACTIONS_RUNTIME_TOKEN,scope=w08"))?;
    assert!(first.contains("COPY a.txt") && !first.contains("CACHED"), "the first build builds: {first}");
    let lower = first.to_ascii_lowercase();
    assert!(lower.contains("exporting") && lower.contains("github actions"), "and exports its cache: {first}");
    assert!(all_cached(&second, &LAYERS), "the second, on a builder of its own, takes every layer from kiln's cache: {second}");
    Ok(())
}

/// W08-R11's `type=registry`: the cache as an image in kiln's registry (W12), the job's own token
/// its credentials (W12-R03), over plain HTTP here.
#[test]
fn w08_r11_buildkit_registry_cache() -> anyhow::Result<()> {
    if !buildkit_here() {
        eprintln!("w08_r11_buildkit_registry_cache: skipped, no Docker with buildx and moby/buildkit:buildx-stable-1 here");
        return Ok(());
    }
    let [(_, first), (_, second)] = twice(&workflow("type=registry,ref=$ref,registry.insecure=true"))?;
    assert!(first.contains("COPY a.txt") && !first.contains("CACHED"), "the first build builds: {first}");
    assert!(first.to_ascii_lowercase().contains("exporting cache"), "and exports its cache: {first}");
    assert!(all_cached(&second, &LAYERS), "the second, on a builder of its own, takes every layer from the registry: {second}");
    Ok(())
}
