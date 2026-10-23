//! W15-R07, AC04 (docs/ci.md): previews. A diff makes its own environment from trunk's
//! `previews:`, its job deploys there (a Compose project of its own), a new version updates it,
//! and landing removes it: its deployment marked inactive and its containers taken down on the
//! runner that made them. A run for no change can't deploy to one. Needs Docker with Compose, and
//! pulls `alpine:3.20`.

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

/// Docker with Compose here.
fn compose() -> bool {
    Command::new("docker").args(["compose", "version"]).output().is_ok_and(|o| o.status.success())
}

/// The containers of Compose project `project`, as `docker ps -a` lists them: `(id, its VERSION)`.
fn containers(project: &str) -> anyhow::Result<Vec<(String, String)>> {
    let ps = Command::new("docker").args(["ps", "-aq", "--filter", &format!("label=com.docker.compose.project={project}")]).output()?;
    let mut out = Vec::new();
    for id in String::from_utf8_lossy(&ps.stdout).split_whitespace() {
        let env = Command::new("docker").args(["inspect", "--format", "{{range .Config.Env}}{{println .}}{{end}}", id]).output()?;
        let version = String::from_utf8_lossy(&env.stdout).lines().find_map(|l| l.strip_prefix("VERSION=")).unwrap_or_default().to_owned();
        out.push((id.to_owned(), version));
    }
    Ok(out)
}

/// What a test deployed, taken down when it ends, whatever happened.
struct Project(String);

impl Drop for Project {
    fn drop(&mut self) {
        for (id, _) in containers(&self.0).unwrap_or_default() {
            let _ = Command::new("docker").args(["rm", "-f", &id]).output();
        }
    }
}

fn compose_file(version: &str) -> String {
    format!(
        "services:\n  web:\n    image: alpine:3.20\n    environment:\n      VERSION: {version}\n    command: [\"sh\", \"-c\", \"touch /tmp/up && exec sleep 3600\"]\n    healthcheck:\n      test: [\"CMD\", \"test\", \"-f\", \"/tmp/up\"]\n      interval: 1s\n      retries: 60\n"
    )
}

/// The newest run of `repo` for `event` that isn't one of `seen`, once it's finished.
fn finished_run(s: &Server, repo: &str, event: &str, seen: &[String]) -> anyhow::Result<String> {
    let mut id = String::new();
    until(s, 600, &format!("a {event} run"), || {
        let (_, body) = http(s.port, "GET", &format!("/api/ci/runs?repo={repo}&limit=20"), &s.admin, None)?;
        let runs: Value = serde_json::from_str(&body)?;
        let run = runs
            .get("runs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|r| r.get("event").and_then(Value::as_str) == Some(event) && !seen.iter().any(|x| Some(x.as_str()) == r.get("id").and_then(Value::as_str)))
            .cloned();
        let Some(r) = run else { return Ok(Some(body)) };
        id = r.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok(r.get("finished_at").is_none_or(Value::is_null).then_some(body))
    })?;
    Ok(id)
}

/// A run's legs by key: `(the leg, its step logs)`.
fn legs(s: &Server, run: &str) -> anyhow::Result<std::collections::BTreeMap<String, (Value, String)>> {
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
    let mut out = std::collections::BTreeMap::new();
    for l in serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
        let id = l.get("id").and_then(Value::as_str).unwrap_or_default();
        let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{id}"), &s.admin, None)?;
        let about: Value = serde_json::from_str(&about)?;
        let mut log = String::new();
        for x in about.get("steps").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.get("idx").and_then(Value::as_u64)) {
            log.push_str(&http(s.port, "GET", &format!("/api/ci/legs/{id}/logs?step={x}"), &s.admin, None)?.1);
        }
        let key = about.pointer("/leg/key").and_then(Value::as_str).unwrap_or_default().to_owned();
        out.insert(key, (about, log));
    }
    Ok(out)
}

/// The repository's environment `name`, as `/api/ci/environments` lists it.
fn environment(s: &Server, repo: &str, name: &str) -> anyhow::Result<Value> {
    let (_, body) = http(s.port, "GET", &format!("/api/ci/environments?repo={repo}"), &s.admin, None)?;
    Ok(serde_json::from_str::<Value>(&body)?
        .get("environments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|e| e.get("name").and_then(Value::as_str) == Some(name))
        .cloned()
        .unwrap_or(Value::Null))
}

#[test]
fn w15_ac04_a_diff_makes_updates_and_lands_its_preview() -> anyhow::Result<()> {
    if !compose() {
        eprintln!("skipped: no Docker with Compose here");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    // A repository of this run's own, so its preview's project is too.
    let name = format!("pv{}", std::process::id());
    let repo = format!("acme/{name}");
    let project = Project(format!("{name}-preview-d1"));
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": repo, "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/{repo}.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "web\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    // The trunk: a preview for each diff, deployed by its pull request's job; and a push's job
    // naming it, which can't.
    std::fs::write(
        work.join(".kiln/environments.yaml"),
        "previews:\n  preview:\n    kind: container\n    runners: label local\n    per: diff\n    ttl: 1h\n    url: http://preview.test/${{ preview.id }}\n",
    )?;
    std::fs::write(work.join(".kiln/review.yaml"), "land:\n  approvals: 0\n")?;
    std::fs::write(work.join("compose.yaml"), compose_file("v1"))?;
    std::fs::write(
        work.join(".kiln/preview.yaml"),
        "on: pull_request\njobs:\n  deploy:\n    runs-on: local\n    environment: preview\n    steps:\n      - uses: kiln/deploy@v1\n        with:\n          environment: preview\n          compose: compose.yaml\n          timeout: 5m\n      - run: echo \"bound to $KILN_PREVIEW for $KILN_PREVIEW_ID\"\n",
    )?;
    std::fs::write(work.join(".kiln/push.yaml"), "on: push\njobs:\n  nowhere:\n    runs-on: local\n    environment: preview\n    steps:\n      - run: echo never\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "previews"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let pushed = finished_run(&s, &repo, "push", &[])?;
    let (about, log) = legs(&s, &pushed)?.remove("nowhere").unwrap_or_default();
    assert_eq!(about.pointer("/leg/conclusion"), Some(&json!("failure")), "a push can't deploy to a preview: {about}\n{log}");
    assert!(about.to_string().contains("this run is for none of them"), "saying why: {about}");

    // D1 v1: its preview made, deployed and bound.
    git(&home, &work, &["checkout", "-qb", "change"])?;
    std::fs::write(work.join("README.md"), "web, changed\n")?;
    git(&home, &work, &["commit", "-qam", "change the web\n\nChange-Id: I1111111111111111111111111111111111111111"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:refs/for/main"])?;
    let v1 = finished_run(&s, &repo, "pull_request", &[])?;
    let (about, log) = legs(&s, &v1)?.remove("deploy").unwrap_or_default();
    assert_eq!(about.pointer("/leg/conclusion"), Some(&json!("success")), "{about}\n{log}\n{}", s.log());
    assert!(log.contains(&format!("Deploying {} to preview-D1", project.0)) && log.contains("bound to preview-D1 for D1"), "{log}");
    let env = environment(&s, &repo, "preview-D1")?;
    assert_eq!(env.pointer("/preview/template"), Some(&json!("preview")), "{env}");
    assert_eq!(env.pointer("/preview/of"), Some(&json!("D1")), "{env}");
    assert_eq!(env.get("url"), Some(&json!("http://preview.test/D1")), "{env}");
    assert_eq!(env.get("kind"), Some(&json!("container")), "{env}");
    assert_eq!(env.pointer("/latest_deployment/state"), Some(&json!("success")), "{env}");
    let up = containers(&project.0)?;
    assert!(up.len() == 1 && up.iter().all(|(_, v)| v == "v1"), "v1 deployed: {up:?}");

    // D1 v2: the same preview, updated.
    std::fs::write(work.join("compose.yaml"), compose_file("v2"))?;
    git(&home, &work, &["commit", "-q", "--amend", "--no-edit", "-a"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:refs/for/main"])?;
    let v2 = finished_run(&s, &repo, "pull_request", &[v1])?;
    let (about, log) = legs(&s, &v2)?.remove("deploy").unwrap_or_default();
    assert_eq!(about.pointer("/leg/conclusion"), Some(&json!("success")), "{about}\n{log}\n{}", s.log());
    let up = containers(&project.0)?;
    assert!(up.len() == 1 && up.iter().all(|(_, v)| v == "v2"), "v2 in v1's place: {up:?}");

    // Landed: the preview goes, and so do its containers.
    let (st, body) = http(s.port, "POST", "/api/diffs/D1/land", &s.admin, Some(&json!({})))?;
    anyhow::ensure!((200..300).contains(&st), "land: {st} {body}");
    until(&s, 300, "D1 lands", || {
        let (_, body) = http(s.port, "GET", "/api/diffs/D1", &s.admin, None)?;
        Ok((serde_json::from_str::<Value>(&body)?.get("state").and_then(Value::as_str) != Some("landed")).then_some(body))
    })?;
    until(&s, 180, "the preview's containers taken down", || {
        let left = containers(&project.0)?;
        Ok((!left.is_empty()).then(|| format!("{left:?}")))
    })?;
    let env = environment(&s, &repo, "preview-D1")?;
    assert_eq!(env.pointer("/preview/removed"), Some(&json!("D1 landed")), "kept a while, saying why: {env}");
    assert_eq!(env.pointer("/latest_deployment/state"), Some(&json!("inactive")), "{env}");
    until(&s, 60, "the runner's word that it took them down", || Ok((!s.log().contains("a runner took a preview's deployment down")).then(|| "not yet".to_owned())))?;
    Ok(())
}
