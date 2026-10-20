//! W13-R05 (docs/ci.md): a leg on the `firecracker` executor runs in a Firecracker microVM of its
//! own. It needs KVM, Firecracker and a guest kernel, so it runs only when `KILN_TEST_FIRECRACKER`
//! and `KILN_TEST_FIRECRACKER_KERNEL` name them (Firecracker's release binary and one of its CI
//! kernels, as `kiln runner setup firecracker` fetches them), and pulls `alpine:3.20` from Docker
//! Hub.

#![cfg(target_os = "linux")]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, http, start_env};
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

/// Firecracker and its kernel, when this machine has them for the test.
fn machine() -> Option<(PathBuf, PathBuf)> {
    let file = |k: &str| std::env::var_os(k).map(PathBuf::from).filter(|p| p.is_file());
    let (fc, kernel) = (file("KILN_TEST_FIRECRACKER")?, file("KILN_TEST_FIRECRACKER_KERNEL")?);
    std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").ok()?;
    Some((fc, kernel))
}

const WORKFLOW: &str = r#"on: push
jobs:
  vm:
    runs-on: microvm
    steps:
      - shell: sh
        run: |
          echo "release=$(cat /proc/sys/kernel/osrelease)"
          echo "executor=$KILN_EXECUTOR"
          echo "shadow-group=$(stat -c %g /etc/shadow)"
          echo "server=$(wget -q -O - "$(echo "$GITHUB_SERVER_URL" | cut -d/ -f1-3)/healthz" 2>&1 | head -c 40)"
          echo "proxy=$(wget -q -O - http://127.0.0.2:9/ 2>&1 | head -c 120)"
"#;

/// A job routed to the `firecracker` executor with `alpine:3.20`: it runs in a microVM (the guest's
/// kernel, not this machine's), on a root file system built from the image with its owners kept
/// (`/etc/shadow`'s group is Alpine's `shadow`, 42), reaching the server on its loopback port and
/// everything else only through this runner's egress proxy (which refuses this machine's own
/// addresses).
#[test]
fn w13_r05_a_leg_runs_in_a_firecracker_microvm() -> anyhow::Result<()> {
    let Some((fc, kernel)) = machine() else {
        eprintln!("skipped: KILN_TEST_FIRECRACKER and KILN_TEST_FIRECRACKER_KERNEL don't name Firecracker and its kernel, or /dev/kvm can't be opened");
        return Ok(());
    };
    // Beside the build (not tmpfs): kiln's own disk is made from this build's binary.
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let rule = "[[ci.routing]]\nmatch = { labels = [\"microvm\"] }\nexecutor = [\"firecracker\"]\nimage = \"docker.io/library/alpine:3.20\"\n";
    let env = [("KILN_FIRECRACKER", fc.to_string_lossy().into_owned()), ("KILN_FIRECRACKER_KERNEL", kernel.to_string_lossy().into_owned())];
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let s = start_env(dir.path(), &Options { agent: true, ci: "", extra: rule }, false, &env)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/vm", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/vm.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "vm\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        if !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the first poll: {body}");
        std::thread::sleep(Duration::from_millis(500));
    }
    std::fs::write(work.join(".github/workflows/vm.yml"), WORKFLOW)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "vm"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    // The first run makes kiln's disk and the image's root file system too.
    let deadline = Instant::now() + Duration::from_secs(900);
    let run = loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/vm&limit=5", &s.admin, None)?;
        let run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0").cloned().unwrap_or_default();
        if run.get("finished_at").is_some_and(|f| !f.is_null()) {
            break run;
        }
        anyhow::ensure!(Instant::now() < deadline, "the run didn't finish: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_secs(1));
    };
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut log = String::new();
    for step in 0..4 {
        let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
        log.push_str(&text);
    }
    let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
    assert_eq!(run.get("conclusion"), Some(&json!("success")), "{about}\n{log}\n{}", s.log());
    let host = std::fs::read_to_string("/proc/sys/kernel/osrelease")?;
    let release = log.lines().find_map(|l| l.split("release=").nth(1)).unwrap_or_default().trim().to_owned();
    assert!(!release.is_empty() && release != host.trim(), "the guest's kernel, not this machine's ({}): {log}", host.trim());
    assert!(log.contains("executor=firecracker"), "{log}");
    assert!(log.contains("shadow-group=42"), "the image's owners kept: {log}");
    assert!(log.lines().any(|l| l.contains("server=") && l.contains("ok")), "the server on the guest's loopback: {log}");
    assert!(log.lines().any(|l| l.contains("proxy=") && l.contains("502")), "out through the egress proxy, which refuses this machine's addresses: {log}");
    Ok(())
}
