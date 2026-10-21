//! W13-R05 (docs/ci.md): a leg on the `firecracker` executor runs in a Firecracker microVM of its
//! own; W17 in it (a kept leg, a terminal into it, a step rerun in place). They need KVM,
//! Firecracker and a guest kernel, so they run only when `KILN_TEST_FIRECRACKER` and
//! `KILN_TEST_FIRECRACKER_KERNEL` name them (Firecracker's release binary and one of its CI
//! kernels, as `kiln runner setup firecracker` fetches them), and pull `alpine:3.20` from Docker
//! Hub.

#![cfg(target_os = "linux")]

mod common;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{Options, Server, http, start_env};
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

/// Firecracker and its kernel, when this machine has them for the test.
fn machine() -> Option<(PathBuf, PathBuf)> {
    let file = |k: &str| std::env::var_os(k).map(PathBuf::from).filter(|p| p.is_file());
    let (fc, kernel) = (file("KILN_TEST_FIRECRACKER")?, file("KILN_TEST_FIRECRACKER_KERNEL")?);
    std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").ok()?;
    Some((fc, kernel))
}

/// A server whose own runner runs `runs-on: microvm` legs in microVMs of `alpine:3.20` (and what
/// `more` configures), with repository `acme/vm` pushed once; its git home, work tree and push URL.
fn vm_server(dir: &Path, fc: &Path, kernel: &Path, more: &str) -> anyhow::Result<(Server, PathBuf, PathBuf, String)> {
    let rule = format!("[[ci.routing]]\nmatch = {{ labels = [\"microvm\"] }}\nexecutor = [\"firecracker\"]\nimage = \"docker.io/library/alpine:3.20\"\n{more}");
    let env = [("KILN_FIRECRACKER", fc.to_string_lossy().into_owned()), ("KILN_FIRECRACKER_KERNEL", kernel.to_string_lossy().into_owned())];
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let s = start_env(dir, &Options { agent: true, ci: "kiln_workflows = true", extra: &rule }, false, &env)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/vm", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.join("githome"), dir.join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&work)?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/vm.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "vm\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    Ok((s, home, work, push))
}

/// `content` committed at `path` and pushed; the run it starts, once it has a leg: its id and its
/// first leg's.
fn push_run(s: &Server, home: &Path, work: &Path, push: &str, path: &str, content: &str) -> anyhow::Result<(String, String)> {
    let file = work.join(path);
    if let Some(d) = file.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&file, content)?;
    git(home, work, &["add", "."])?;
    git(home, work, &["commit", "-qm", path])?;
    git(home, work, &["push", "-q", push, "HEAD:main"])?;
    let (mut run, mut leg) = (String::new(), String::new());
    until(s, 120, "the run's leg", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/vm&limit=5", &s.admin, None)?;
        run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        if run.is_empty() {
            return Ok(Some(body));
        }
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
        leg = serde_json::from_str::<Value>(&detail).unwrap_or_default().pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok(leg.is_empty().then_some(detail))
    })?;
    Ok((run, leg))
}

/// A leg's detail and its steps' logs.
fn leg_and_log(s: &Server, leg: &str) -> anyhow::Result<(Value, String)> {
    let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
    let about: Value = serde_json::from_str(&about)?;
    let mut log = String::new();
    for step in about.get("steps").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.get("idx").and_then(Value::as_u64)) {
        let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
        log.push_str(&text);
    }
    Ok((about, log))
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
    let (s, home, work, push) = vm_server(dir.path(), &fc, &kernel, "")?;
    // The first run makes kiln's disk and the image's root file system too.
    let (run, leg) = push_run(&s, &home, &work, &push, ".github/workflows/vm.yml", WORKFLOW)?;
    until(&s, 900, "the run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/vm&limit=5", &s.admin, None)?;
        let runs: Value = serde_json::from_str(&body)?;
        let done = runs.get("runs").and_then(Value::as_array).into_iter().flatten().any(|r| r.get("id") == Some(&json!(run)) && r.get("finished_at").is_some_and(|f| !f.is_null()));
        Ok((!done).then_some(body))
    })?;
    let (about, log) = leg_and_log(&s, &leg)?;
    assert_eq!(about.pointer("/leg/conclusion"), Some(&json!("success")), "{about}\n{log}\n{}", s.log());
    let host = std::fs::read_to_string("/proc/sys/kernel/osrelease")?;
    let release = log.lines().find_map(|l| l.split("release=").nth(1)).unwrap_or_default().trim().to_owned();
    assert!(!release.is_empty() && release != host.trim(), "the guest's kernel, not this machine's ({}): {log}", host.trim());
    assert!(log.contains("executor=firecracker"), "{log}");
    assert!(log.contains("shadow-group=42"), "the image's owners kept: {log}");
    assert!(log.lines().any(|l| l.contains("server=") && l.contains("ok")), "the server on the guest's loopback: {log}");
    assert!(log.lines().any(|l| l.contains("proxy=") && l.contains("502")), "out through the egress proxy, which refuses this machine's addresses: {log}");
    Ok(())
}

const KEPT: &str = r#"on: push
jobs:
  kept:
    runs-on: microvm
    checkout: false
    debug: { keep-on-failure: 5m }
    env:
      WORD: ${{ secrets.WORD }}
    steps:
      - shell: sh
        run: test -f "$GITHUB_WORKSPACE/fixed" && echo fixed-now
"#;

/// W17 in a microVM: a failed leg with `keep-on-failure` keeps its microVM; a terminal into it opens
/// in its workspace with its environment, its secret masked; the step, fixed from the terminal,
/// passes when rerun in place; and the release ends the microVM.
#[test]
fn w17_a_kept_leg_in_a_microvm() -> anyhow::Result<()> {
    let Some((fc, kernel)) = machine() else {
        eprintln!("skipped: KILN_TEST_FIRECRACKER and KILN_TEST_FIRECRACKER_KERNEL don't name Firecracker and its kernel, or /dev/kvm can't be opened");
        return Ok(());
    };
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    // Its own runner opens terminals into legs (W17-R01).
    let (s, home, work, push) = vm_server(dir.path(), &fc, &kernel, "\n[terminal]\nenabled = true\n")?;
    let (st, body) = http(s.port, "PUT", "/api/ci/secrets/WORD?scope=repo:acme/vm", &s.admin, Some(&json!({ "value": "plums-in-a-microvm" })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let (_, leg) = push_run(&s, &home, &work, &push, ".kiln/kept.yaml", KEPT)?;
    until(&s, 900, "the failed leg, kept", || {
        let (about, _) = leg_and_log(&s, &leg)?;
        let kept = about.pointer("/leg/kept_until").is_some_and(|k| !k.is_null());
        Ok((!kept || about.pointer("/leg/conclusion") != Some(&json!("failure"))).then(|| about.to_string()))
    })?;
    // A terminal into it: its workspace, its environment, its secret masked.
    let mut child = Command::new(env!("CARGO_BIN_EXE_kiln"))
        .args(["debug", "terminal", &leg])
        .env("HOME", &home)
        .env("KILN_URL", format!("http://127.0.0.1:{}", s.port))
        .env("KILN_ADMIN_TOKEN", &s.admin)
        .env_remove("KILN_CONFIG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(b"touch \"$GITHUB_WORKSPACE/fixed\" && test \"$PWD\" = \"$GITHUB_WORKSPACE\" && echo in-the-work\"\"space\necho \"word=$WORD-$((6*7))\"\nexit\n")?;
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait()?.is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            anyhow::bail!("the terminal didn't end with its shell\n{}", s.log());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let o = child.wait_with_output()?;
    let (out, err) = (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned());
    assert!(o.status.success(), "{out}\n{err}\n{}", s.log());
    assert!(out.contains("in-the-workspace"), "it opens in the leg's workspace: {out}\n{err}");
    assert!(out.contains("word=***-42") && !out.contains("plums-in-a-microvm"), "its environment, its secret masked: {out}\n{err}");
    // The step rerun in place, now that the file it wants is there.
    let (st, body) = http(s.port, "POST", &format!("/api/ci/legs/{leg}/rerun-step"), &s.admin, Some(&json!({ "idx": 0 })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    until(&s, 120, "the rerun", || {
        let (about, log) = leg_and_log(&s, &leg)?;
        let rerun = about.get("steps").and_then(Value::as_array).into_iter().flatten().any(|x| x.get("idx") == Some(&json!(1)) && x.get("conclusion") == Some(&json!("success")));
        Ok((!rerun || !log.contains("fixed-now")).then(|| format!("{about}\n{log}")))
    })?;
    // Released: the microVM ends, and its disks go.
    let (st, body) = http(s.port, "POST", &format!("/api/ci/legs/{leg}/release"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let vms = dir.path().join("work/_firecracker/vms");
    until(&s, 60, "the microVM to end", || {
        let left: Vec<String> = std::fs::read_dir(&vms).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        Ok((!left.is_empty()).then(|| format!("{left:?}")))
    })?;
    Ok(())
}

/// The pool's executors that are booted and waiting for a leg.
fn idle(s: &Server, pool: &str) -> anyhow::Result<Vec<String>> {
    let (_, body) = http(s.port, "GET", &format!("/api/ci/pools/{pool}"), &s.admin, None)?;
    let v: Value = serde_json::from_str(&body)?;
    let list = v.pointer("/pool/executors").or_else(|| v.get("executors")).and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(list.iter().filter(|e| e.get("state") == Some(&json!("idle"))).filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_owned)).collect())
}

/// W13-R09 with R05: a `firecracker` pool keeps a microVM of its image booted and waiting; a leg
/// for the pool runs in it at once, and the pool boots another for the next.
#[test]
fn w13_r09_a_firecracker_pool_keeps_a_microvm_ready() -> anyhow::Result<()> {
    let Some((fc, kernel)) = machine() else {
        eprintln!("skipped: KILN_TEST_FIRECRACKER and KILN_TEST_FIRECRACKER_KERNEL don't name Firecracker and its kernel, or /dev/kvm can't be opened");
        return Ok(());
    };
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let pool = "\n[[ci.pools]]\nname = \"vms\"\nexecutor = \"firecracker\"\nimage = \"docker.io/library/alpine:3.20\"\nlabels = [\"vmpool\"]\nmin_idle = 1\nmax = 2\n";
    let (s, home, work, push) = vm_server(dir.path(), &fc, &kernel, pool)?;
    let mut first = Vec::new();
    until(&s, 900, "a microVM of the pool's, ready", || {
        first = idle(&s, "vms")?;
        Ok(first.is_empty().then(|| "none idle".to_owned()))
    })?;
    let workflow = "on: push\njobs:\n  pooled:\n    runs-on: vmpool\n    steps:\n      - shell: sh\n        run: echo \"pooled-on-$(cat /proc/sys/kernel/osrelease)\"\n";
    let (run, leg) = push_run(&s, &home, &work, &push, ".github/workflows/pooled.yml", workflow)?;
    until(&s, 300, "the pooled run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/vm&limit=5", &s.admin, None)?;
        let runs: Value = serde_json::from_str(&body)?;
        let done = runs.get("runs").and_then(Value::as_array).into_iter().flatten().any(|r| r.get("id") == Some(&json!(run)) && r.get("finished_at").is_some_and(|f| !f.is_null()));
        Ok((!done).then_some(body))
    })?;
    let (about, log) = leg_and_log(&s, &leg)?;
    assert_eq!(about.pointer("/leg/conclusion"), Some(&json!("success")), "{about}\n{log}\n{}", s.log());
    assert!(log.contains("pooled-on-"), "{log}");
    // The microVM it ran in is gone, and another stands ready.
    until(&s, 600, "the pool's next microVM", || {
        let now = idle(&s, "vms")?;
        Ok((now.is_empty() || now.iter().any(|e| first.contains(e))).then(|| format!("{now:?} (first {first:?})")))
    })?;
    Ok(())
}
