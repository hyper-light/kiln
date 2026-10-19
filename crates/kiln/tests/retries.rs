//! Retries by the kind of failure (docs/ci.md W05-R14, R15): the runner says what kind each
//! failed attempt was, and the leg's `retry:` policy retries only the kinds it names. A log line
//! (`{ log: ... }`), a full disk (ENOSPC said), the OOM killer (a job container at its memory
//! limit), and a plain failure, which `on: [oom]` leaves alone.

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

fn docker_works() -> bool {
    Command::new("docker").args(["info", "--format", "{{.ServerVersion}}"]).output().is_ok_and(|o| o.status.success())
}

/// Each job fails its first attempt on purpose, as the kind it's named for.
fn workflow(oom: bool) -> String {
    let mut w = String::from(
        r#"on: push
jobs:
  reset:
    runs-on: local
    retry:
      max: 2
      on: [{ log: "connection reset by peer" }]
      backoff: { initial: 1s }
    steps:
      - run: |
          if [ "$KILN_ATTEMPT" = 1 ]; then echo "curl: (56) Recv failure: connection reset by peer"; exit 1; fi
          echo "reset passed on attempt $KILN_ATTEMPT"
  disk:
    runs-on: local
    retry:
      max: 1
      on: [disk-full]
      backoff: { initial: 1s }
    steps:
      - run: |
          if [ "$KILN_ATTEMPT" = 1 ]; then echo "write error: No space left on device" >&2; exit 1; fi
          echo "disk passed on attempt $KILN_ATTEMPT"
  plain:
    runs-on: local
    retry:
      max: 2
      on: [oom]
      backoff: { initial: 1s }
    steps:
      - run: exit 3
"#,
    );
    if oom {
        w.push_str(
            r#"  oom:
    runs-on: local
    container:
      image: alpine:3.20
      options: --memory 64m --memory-swap 64m
    retry:
      max: 1
      on: [oom]
      backoff: { initial: 1s }
    steps:
      - run: |
          if [ "$KILN_ATTEMPT" = 1 ]; then awk 'BEGIN { s = "x"; while (1) s = s s }'; fi
          echo "oom passed on attempt $KILN_ATTEMPT"
"#,
        );
    }
    w
}

#[test]
fn w05_r15_retries_by_kind() -> anyhow::Result<()> {
    let oom = docker_works();
    let dir = tempfile::tempdir()?;
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo)?;
    std::fs::write(repo.join("README.md"), "retries\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!("[[repos]]\nname = \"acme/retries\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: &extra })?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        if body.contains("acme/retries") && !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::write(repo.join(".kiln/retries.yaml"), workflow(oom))?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "retries"])?;
    let deadline = Instant::now() + Duration::from_secs(600);
    let legs = loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/retries&limit=5", &s.admin, None)?;
        let runs: Value = serde_json::from_str(&body).unwrap_or_default();
        if let Some(id) = runs.pointer("/runs/0/id").and_then(Value::as_str)
            && runs.pointer("/runs/0/finished_at").is_some_and(|f| !f.is_null())
        {
            let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
            break serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default();
        }
        anyhow::ensure!(Instant::now() < deadline, "the run didn't finish: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let leg = |key: &str| legs.iter().find(|l| l.get("key").and_then(Value::as_str) == Some(key)).cloned().unwrap_or_default();
    let said = |key: &str, what: &str| -> anyhow::Result<bool> {
        let id = leg(key).get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let (_, log) = http(s.port, "GET", &format!("/api/ci/legs/{id}/logs?step=0"), &s.admin, None)?;
        Ok(log.contains(what))
    };
    let state = |key: &str| {
        let l = leg(key);
        (l.get("conclusion").and_then(Value::as_str).unwrap_or_default().to_owned(), l.get("attempt").and_then(Value::as_i64).unwrap_or(0))
    };
    assert_eq!(state("reset"), ("success".to_owned(), 2), "a log line the policy names: retried\n{legs:?}\n{}", s.log());
    assert!(said("reset", "reset passed on attempt 2")?);
    assert_eq!(state("disk"), ("success".to_owned(), 2), "a full disk: retried\n{legs:?}");
    assert!(said("disk", "disk passed on attempt 2")?);
    assert_eq!(state("plain"), ("failure".to_owned(), 1), "a plain failure isn't the oom the policy retries\n{legs:?}");
    if oom {
        assert_eq!(state("oom"), ("success".to_owned(), 2), "the OOM killer: retried\n{legs:?}\n{}", s.log());
        assert!(said("oom", "oom passed on attempt 2")?);
    } else {
        eprintln!("w05_r15_retries_by_kind: no Docker, so no OOM case");
    }
    Ok(())
}
