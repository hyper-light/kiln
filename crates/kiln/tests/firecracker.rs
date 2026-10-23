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
    let newest = || -> anyhow::Result<(String, String)> {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/vm&limit=5", &s.admin, None)?;
        let run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok((run, body))
    };
    let (before, _) = newest()?;
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
        let body;
        (run, body) = newest()?;
        // This push's run, not one before it.
        if run.is_empty() || run == before {
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

/// W13-R09 with R05: a `firecracker` pool keeps a microVM of its image ready, and a leg for the
/// pool runs in it at once, the pool readying another for the next. Where its VMMs are jailed,
/// each is restored from a snapshot of its image booted once: its clock is the host's, not the
/// snapshot's, and its randomness its own, not its siblings'.
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
    let workflow = "on: push\njobs:\n  pooled:\n    runs-on: vmpool\n    strategy:\n      matrix:\n        n: [1, 2]\n      max-parallel: 1\n    steps:\n      - shell: sh\n        run: echo \"pooled-on-$(cat /proc/sys/kernel/osrelease) at $(date +%s) random $(head -c 16 /dev/urandom | od -An -tx1 | tr -d \' \\n\')\"\n";
    let (run, _) = push_run(&s, &home, &work, &push, ".github/workflows/pooled.yml", workflow)?;
    until(&s, 600, "the pooled run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/vm&limit=5", &s.admin, None)?;
        let runs: Value = serde_json::from_str(&body)?;
        let done = runs.get("runs").and_then(Value::as_array).into_iter().flatten().any(|r| r.get("id") == Some(&json!(run)) && r.get("finished_at").is_some_and(|f| !f.is_null()));
        Ok((!done).then_some(body))
    })?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
    let mut randoms = Vec::new();
    for l in serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
        let (about, log) = leg_and_log(&s, l.get("id").and_then(Value::as_str).unwrap_or_default())?;
        assert_eq!(about.pointer("/leg/conclusion"), Some(&json!("success")), "{about}\n{log}\n{}", s.log());
        let line = log.lines().find(|l| l.contains("pooled-on-")).unwrap_or_default().to_owned();
        let word = |after: &str| line.split(after).nth(1).and_then(|r| r.split_whitespace().next()).unwrap_or_default().to_owned();
        let at: u64 = word(" at ").parse().unwrap_or(0);
        assert!(at + 600 >= now && at <= now + 5, "the guest's clock is the host's ({at}, now {now}): {line}");
        randoms.push(word(" random "));
    }
    assert_eq!(randoms.len(), 2, "{randoms:?}");
    assert!(randoms.iter().all(|r| r.len() == 32) && randoms.first() != randoms.get(1), "each microVM's randomness its own: {randoms:?}");
    // Restored from the image's snapshot where its VMMs are jailed.
    if namespaces() {
        let log = std::fs::read_to_string(dir.path().join("server.log")).unwrap_or_default();
        assert!(log.lines().any(|l| l.contains("pool microVM ready") && l.contains("restored=true")), "restored from a snapshot:\n{}", s.log());
    }
    // The microVM it ran in is gone, and another stands ready.
    until(&s, 600, "the pool's next microVM", || {
        let now = idle(&s, "vms")?;
        Ok((now.is_empty() || now.iter().any(|e| first.contains(e))).then(|| format!("{now:?} (first {first:?})")))
    })?;
    Ok(())
}

/// Whether this machine allows the jail's namespaces (a jailed VMM's, `jail.rs`).
fn namespaces() -> bool {
    Command::new("unshare").args(["-Urmpnf", "--mount-proc", "true"]).output().is_ok_and(|o| o.status.success())
}

/// W13-R05's last part: in a microVM whose image has Docker (`docker:dind`), the guest starts its
/// daemon, and a Docker action runs there (on the guest's kernel), its image pulled through the
/// egress proxy.
#[test]
fn w13_r05_docker_actions_in_a_microvm_with_docker() -> anyhow::Result<()> {
    let Some((fc, kernel)) = machine() else {
        eprintln!("skipped: KILN_TEST_FIRECRACKER and KILN_TEST_FIRECRACKER_KERNEL don't name Firecracker and its kernel, or /dev/kvm can't be opened");
        return Ok(());
    };
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let docker = "\n[[ci.routing]]\nmatch = { labels = [\"dockervm\"] }\nexecutor = [\"firecracker\"]\nimage = \"docker.io/library/docker:dind\"\n";
    let (s, home, work, push) = vm_server(dir.path(), &fc, &kernel, docker)?;
    let workflow =
        "on: push\njobs:\n  docker:\n    runs-on: dockervm\n    steps:\n      - uses: docker://alpine:3.20\n        with:\n          args: sh -c \"echo from-a-docker-action-on-$(uname -r)\"\n";
    let (run, leg) = push_run(&s, &home, &work, &push, ".github/workflows/docker.yml", workflow)?;
    until(&s, 1200, "the run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/vm&limit=5", &s.admin, None)?;
        let runs: Value = serde_json::from_str(&body)?;
        let done = runs.get("runs").and_then(Value::as_array).into_iter().flatten().any(|r| r.get("id") == Some(&json!(run)) && r.get("finished_at").is_some_and(|f| !f.is_null()));
        Ok((!done).then_some(body))
    })?;
    let (about, log) = leg_and_log(&s, &leg)?;
    assert_eq!(about.pointer("/leg/conclusion"), Some(&json!("success")), "{about}\n{log}\n{}", s.log());
    let host = std::fs::read_to_string("/proc/sys/kernel/osrelease")?;
    let said = log.lines().find_map(|l| l.split("from-a-docker-action-on-").nth(1)).unwrap_or_default().trim().to_owned();
    assert!(!said.is_empty() && said != host.trim(), "the action ran in the microVM's Docker, on its kernel: {log}");
    Ok(())
}

/// F-21's `setup-qemu-action` (W22-AC02), which changes the kernel's binfmt and so needs a runner
/// of its own: in a microVM with Docker, it registers its emulators with the guest's kernel, and an
/// arm64 container runs. Needs github.com (the action) and Docker Hub (its images).
#[test]
fn w22_ac02_setup_qemu_in_a_microvm() -> anyhow::Result<()> {
    let Some((fc, kernel)) = machine() else {
        eprintln!("skipped: KILN_TEST_FIRECRACKER and KILN_TEST_FIRECRACKER_KERNEL don't name Firecracker and its kernel, or /dev/kvm can't be opened");
        return Ok(());
    };
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let docker = "\n[[ci.routing]]\nmatch = { labels = [\"dockervm\"] }\nexecutor = [\"firecracker\"]\nimage = \"docker.io/library/docker:dind\"\n";
    let (s, home, work, push) = vm_server(dir.path(), &fc, &kernel, docker)?;
    let workflow = "on: push\njobs:\n  qemu:\n    runs-on: dockervm\n    steps:\n      - uses: docker/setup-qemu-action@v3\n        with:\n          platforms: arm64\n      - run: echo \"machine=$(docker run --rm --platform linux/arm64 alpine:3.20 uname -m)\"\n";
    let (run, leg) = push_run(&s, &home, &work, &push, ".github/workflows/qemu.yml", workflow)?;
    until(&s, 1800, "the run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/vm&limit=5", &s.admin, None)?;
        let runs: Value = serde_json::from_str(&body)?;
        let done = runs.get("runs").and_then(Value::as_array).into_iter().flatten().any(|r| r.get("id") == Some(&json!(run)) && r.get("finished_at").is_some_and(|f| !f.is_null()));
        Ok((!done).then_some(body))
    })?;
    let (about, log) = leg_and_log(&s, &leg)?;
    assert_eq!(about.pointer("/leg/conclusion"), Some(&json!("success")), "{about}\n{log}\n{}", s.log());
    assert!(log.contains("machine=aarch64"), "an arm64 container, emulated in the microVM: {log}");
    Ok(())
}

/// A free port on this machine's loopback.
fn free_port() -> anyhow::Result<u16> {
    Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

/// What `GET /` on this machine's `port` answers (its body), or why it didn't.
fn fetch(port: u16) -> anyhow::Result<String> {
    use std::io::Read as _;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    s.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
    let mut answer = String::new();
    s.read_to_string(&mut answer)?;
    Ok(answer.split_once("\r\n\r\n").map(|(_, body)| body.trim().to_owned()).unwrap_or(answer))
}

/// The microVMs booted from under `dir`: their VMMs (`--config-file <dir>/…/vm.json`), or the jails
/// they run in (`kiln vmm-jail <dir>/…/jail.json`; a jailed VMM's own paths are its jail's).
fn microvms_under(dir: &Path) -> Vec<u32> {
    let under = dir.to_string_lossy().into_owned();
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()))
        .filter(|pid| {
            std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| {
                let c = String::from_utf8_lossy(&c);
                c.contains(&under) && (c.contains("vmm-jail") || (c.contains("--config-file") && c.contains("firecracker")))
            })
        })
        .collect()
}

/// W15-R02 (AC03's `microvm` part): a `microvm` environment's deployment. A job bound to it deploys
/// an image (busybox's `httpd`, serving what its command wrote) as a microVM of its own on this
/// runner: the step waits until it answers its health check, it answers on this runner's port
/// after its job has ended, and the next deployment takes the port over, the one before ending.
/// They end with the runner.
#[test]
fn w15_r02_a_microvm_environment_deploys_an_image() -> anyhow::Result<()> {
    let Some((fc, kernel)) = machine() else {
        eprintln!("skipped: KILN_TEST_FIRECRACKER and KILN_TEST_FIRECRACKER_KERNEL don't name Firecracker and its kernel, or /dev/kvm can't be opened");
        return Ok(());
    };
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let (s, home, work, push) = vm_server(dir.path(), &fc, &kernel, "")?;
    let (st, body) = http(s.port, "PUT", "/api/ci/environments/web?repo=acme/vm", &s.admin, Some(&json!({ "kind": "microvm", "targets": { "runners": "label local" } })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let port = free_port()?;
    let deploy = |says: &str| {
        format!(
            "on: push\njobs:\n  ship:\n    runs-on: local\n    environment: web\n    steps:\n      - uses: kiln/deploy@v1\n        with:\n          environment: web\n          image: docker.io/library/busybox:1.36\n          command: mkdir -p /www && echo {says} > /www/index.html && exec httpd -f -p 8080 -h /www\n          ports: 127.0.0.1:{port}:8080\n          health: http://127.0.0.1:8080/\n          timeout: 10m\n"
        )
    };
    for says in ["one", "two"] {
        let (run, leg) = push_run(&s, &home, &work, &push, ".kiln/ship.yaml", &deploy(says))?;
        until(&s, 900, "the deployment's run", || {
            let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/vm&limit=5", &s.admin, None)?;
            let runs: Value = serde_json::from_str(&body)?;
            let done = runs.get("runs").and_then(Value::as_array).into_iter().flatten().any(|r| r.get("id") == Some(&json!(run)) && r.get("finished_at").is_some_and(|f| !f.is_null()));
            Ok((!done).then_some(body))
        })?;
        let (about, log) = leg_and_log(&s, &leg)?;
        assert_eq!(about.pointer("/leg/conclusion"), Some(&json!("success")), "{about}\n{log}\n{}", s.log());
        assert!(log.contains("Healthy"), "it waited until it was healthy: {log}");
        // After its job, on this runner's port.
        let got = fetch(port).map_err(|e| anyhow::anyhow!("GET / on {port}: {e}\n{log}\n{}", s.log()))?;
        assert_eq!(got, says, "the deployment answers after its job: {log}");
    }
    // The second took over from the first, which ended.
    let vms = microvms_under(dir.path());
    assert_eq!(vms.len(), 1, "one deployment's microVM runs: {vms:?}");
    // And it ends with its runner.
    drop(s);
    let deadline = Instant::now() + Duration::from_secs(90);
    while !microvms_under(dir.path()).is_empty() {
        anyhow::ensure!(Instant::now() < deadline, "the deployment's microVM outlived its runner: {:?}", microvms_under(dir.path()));
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

/// W13-R05 with W06's containers: in a microVM whose image has Docker, a leg's service and its job
/// container run on the guest's Docker, on the guest's network, with a network policy too (the
/// microVM is what contains them: its one way out is this runner's egress proxy). The service
/// answers the leg's steps at its port, and a job container reaches it by its name.
#[test]
fn w13_r05_services_and_a_job_container_in_a_microvm() -> anyhow::Result<()> {
    let Some((fc, kernel)) = machine() else {
        eprintln!("skipped: KILN_TEST_FIRECRACKER and KILN_TEST_FIRECRACKER_KERNEL don't name Firecracker and its kernel, or /dev/kvm can't be opened");
        return Ok(());
    };
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let docker = "\n[[ci.routing]]\nmatch = { labels = [\"dockervm\"] }\nexecutor = [\"firecracker\"]\nimage = \"docker.io/library/docker:dind\"\n";
    let (s, home, work, push) = vm_server(dir.path(), &fc, &kernel, docker)?;
    let ping = "printf 'PING\\r\\n' | nc -w 5 $HOST 6379 | tr -d '\\r'";
    let workflow = format!(
        "on: push\njobs:\n  steps:\n    runs-on: dockervm\n    services:\n      cache:\n        image: docker.io/library/redis:7-alpine\n    steps:\n      - run: |\n          for i in $(seq 1 60); do nc -z 127.0.0.1 6379 && break; sleep 1; done\n          echo \"at its port: $(HOST=127.0.0.1; {ping})\"\n  contained:\n    runs-on: dockervm\n    network: {{ egress: {{ allow: [\"*\"] }} }}\n    container: docker.io/library/alpine:3.20\n    services:\n      cache:\n        image: docker.io/library/redis:7-alpine\n    steps:\n      - run: |\n          for i in $(seq 1 60); do nc -z cache 6379 && break; sleep 1; done\n          echo \"by its name: $(HOST=cache; {ping})\"\n"
    );
    let (run, _) = push_run(&s, &home, &work, &push, ".kiln/services.yaml", &workflow)?;
    until(&s, 1200, "the run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/vm&limit=5", &s.admin, None)?;
        let runs: Value = serde_json::from_str(&body)?;
        let done = runs.get("runs").and_then(Value::as_array).into_iter().flatten().any(|r| r.get("id") == Some(&json!(run)) && r.get("finished_at").is_some_and(|f| !f.is_null()));
        Ok((!done).then_some(body))
    })?;
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
    for l in serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
        let (id, key) = (l.get("id").and_then(Value::as_str).unwrap_or_default(), l.get("key").and_then(Value::as_str).unwrap_or_default().to_owned());
        let (about, log) = leg_and_log(&s, id)?;
        assert_eq!(about.pointer("/leg/conclusion"), Some(&json!("success")), "{key}: {about}\n{log}\n{}", s.log());
        assert_eq!(about.pointer("/leg/executor"), Some(&json!("firecracker")), "{key} ran in a microVM: {about}");
        let said = if key == "contained" { "by its name: +PONG" } else { "at its port: +PONG" };
        assert!(log.contains(said), "{key}: {log}");
    }
    Ok(())
}

/// W13-AC02 on `firecracker` (with W06-AC01): F-20, the toolkit conformance action, in a microVM of
/// `alpine:3.20`, its JavaScript on the musl Node W07-R05 fetches for it. Each effect is as on the
/// host executor: its inputs, the secret it masks, `exec` and `io`, its outputs, variable and path
/// in a later step, its post step's state, and `getIDToken` from kiln's issuer, masked.
#[test]
fn w13_ac02_toolkit_conformance_in_a_microvm() -> anyhow::Result<()> {
    let Some((fc, kernel)) = machine() else {
        eprintln!("skipped: KILN_TEST_FIRECRACKER and KILN_TEST_FIRECRACKER_KERNEL don't name Firecracker and its kernel, or /dev/kvm can't be opened");
        return Ok(());
    };
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let (s, home, work, push) = vm_server(dir.path(), &fc, &kernel, "")?;
    // The action, the repository's own.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/toolkit-action");
    let to = work.join(".github/actions/toolkit");
    std::fs::create_dir_all(to.join("dist"))?;
    std::fs::copy(fixture.join("action.yml"), to.join("action.yml"))?;
    for f in ["main.mjs", "post.mjs", "shared.mjs"] {
        std::fs::copy(fixture.join("dist").join(f), to.join("dist").join(f))?;
    }
    let workflow = r#"on: push
permissions:
  contents: read
  id-token: write
jobs:
  t:
    runs-on: microvm
    steps:
      - uses: kiln/checkout@v1
      - id: t
        uses: ./.github/actions/toolkit
        with:
          required: yes-req
      - run: |
          echo "o1=${{ steps.t.outputs.o1 }} EXPORTED=$EXPORTED"
          case "$PATH" in /opt/f20-bin:*) echo "path first" ;; esac
          echo "the secret again: super-secret-f20"
"#;
    let (run, leg) = push_run(&s, &home, &work, &push, ".github/workflows/toolkit.yml", workflow)?;
    until(&s, 1200, "the run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/vm&limit=5", &s.admin, None)?;
        let runs: Value = serde_json::from_str(&body)?;
        let done = runs.get("runs").and_then(Value::as_array).into_iter().flatten().any(|r| r.get("id") == Some(&json!(run)) && r.get("finished_at").is_some_and(|f| !f.is_null()));
        Ok((!done).then_some(body))
    })?;
    let (about, log) = leg_and_log(&s, &leg)?;
    assert_eq!(about.pointer("/leg/conclusion"), Some(&json!("success")), "{about}\n{log}\n{}", s.log());
    assert_eq!(about.pointer("/leg/executor"), Some(&json!("firecracker")), "{about}");
    assert!(log.contains(r#"inputs: required=yes-req missing=[] padded=[spaced] raw=[  spaced  ] flag=true lines=["a","b","c"]"#), "{log}");
    assert!(log.contains("required absent: Input required and not supplied: absent"), "{log}");
    assert!(log.contains("printing the secret: ***") && !log.contains("super-secret-f20"), "setSecret masks it from then on: {log}");
    assert!(log.contains("exec output: captured (exit 0)") && log.contains("which sh: true"), "{log}");
    assert!(log.contains(r#"o1=v1 EXPORTED=e1"#) && log.contains("path first") && log.contains("the secret again: ***"), "{log}");
    assert!(log.contains("post: state=state-v"), "the post step reads the state: {log}");
    assert!(log.contains("id token: aud=kiln-f20 sub=repo:acme/vm:ref:refs/heads/main"), "kiln's issuer, for its audience: {log}");
    assert!(log.contains("the token itself: ***"), "the token is masked: {log}");
    Ok(())
}
