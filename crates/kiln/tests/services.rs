//! W13-AC03 (docs/ci.md): a Postgres service with a health check, the leg's steps starting once
//! it's healthy. A job on the host reaches it at `localhost` on its published port; a job container
//! reaches it by its name, and so does a leg in the `docker` executor. (A `k8s` pod's service, a
//! sidecar, is tests/k8s_pool.rs's.) Needs Docker, and pulls `postgres:16-alpine`.

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

const SERVICE: &str = r#"      db:
        image: docker.io/library/postgres:16-alpine
        env:
          POSTGRES_PASSWORD: pw
        ports: ["5432"]
        options: >-
          --health-cmd "pg_isready -U postgres" --health-interval 1s --health-timeout 5s --health-retries 60
"#;

/// W13-AC03 on the host executor (a job of its own and a job container) and the `docker` executor.
#[test]
fn w13_ac03_a_postgres_service_with_a_health_check() -> anyhow::Result<()> {
    if !Command::new("docker").arg("version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: no Docker here");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let rule = "[[ci.routing]]\nmatch = { labels = [\"pgexec\"] }\nexecutor = [\"docker\"]\nimage = \"docker.io/library/postgres:16-alpine\"\n";
    let s = start(dir.path(), &Options { agent: true, ci: "", extra: rule })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/db", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/db.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "db\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    let query = "PGPASSWORD=pw psql -h db -U postgres -tAc 'select 40 + 2'";
    let workflow = format!(
        "on: push\njobs:\n  host:\n    runs-on: local\n    services:\n{SERVICE}    steps:\n      - run: |\n          port=${{{{ job.services.db.ports['5432'] }}}}\n          (exec 3<>\"/dev/tcp/127.0.0.1/$port\") && echo \"by localhost: open on $port\"\n  container:\n    runs-on: local\n    container: docker.io/library/postgres:16-alpine\n    services:\n{SERVICE}    steps:\n      - run: |\n          echo \"by its name: $({query})\"\n  executor:\n    runs-on: pgexec\n    services:\n{SERVICE}    steps:\n      - run: |\n          echo \"in the docker executor: $({query})\"\n"
    );
    std::fs::write(work.join(".github/workflows/services.yml"), workflow)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "services"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let mut run = Value::Null;
    until(&s, 900, "the run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/db&limit=5", &s.admin, None)?;
        run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0").cloned().unwrap_or_default();
        Ok(run.get("finished_at").is_none_or(Value::is_null).then_some(body))
    })?;
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let mut said = std::collections::BTreeMap::new();
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
        said.insert(key, (about.pointer("/leg/executor").cloned().unwrap_or_default(), log));
    }
    anyhow::ensure!(said.len() == 3, "the run's three legs: {run}\n{detail}\n{}", s.log());
    let (_, host) = said.get("host").cloned().unwrap_or_default();
    assert!(host.contains("by localhost: open on"), "{host}");
    let (_, container) = said.get("container").cloned().unwrap_or_default();
    assert!(container.contains("by its name: 42"), "{container}");
    let (executor, inside) = said.get("executor").cloned().unwrap_or_default();
    assert_eq!(executor, json!("docker"), "{inside}");
    assert!(inside.contains("in the docker executor: 42"), "{inside}");
    Ok(())
}
