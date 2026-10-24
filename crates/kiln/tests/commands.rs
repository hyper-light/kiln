//! Plan step 6 of "Nothing kiln sends between its processes is fire-and-forget" (docs/wire.md,
//! "Commands"): the server's commands to a runner ride that runner's command streams and reach
//! their callers as the runner's answers.
//!
//! A failed leg is kept on runner `keeper`, whose link is cut every 60 s for 15 s, while about one
//! in two of the server's channel frames to its runners never reach the wire. A step rerun in
//! place (a live command) is answered through the lost frames, and the leg's step passes. While the
//! keeper is away, a rerun is refused at once (unreachable), and a release (a parked command) is
//! answered parked: it reaches the keeper once its link is back, and the kept workspace goes.
//!
//! `cargo test -p kiln --features chaos --test commands`

#![cfg(all(unix, feature = "chaos"))]

mod common;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{Options, Server, http, start_env};
use serde_json::{Value, json};

struct Keeper(Child);

impl Drop for Keeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let o = Command::new("git").arg("-C").arg(dir).args(args).env("GIT_AUTHOR_NAME", "T").env("GIT_AUTHOR_EMAIL", "t@example.com").env("GIT_COMMITTER_NAME", "T").env("GIT_COMMITTER_EMAIL", "t@example.com").output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(())
}

/// Waits until `f` says nothing is left to wait for (None), at most `secs`.
fn until(s: &Server, secs: u64, what: &str, mut f: impl FnMut() -> anyhow::Result<Option<String>>) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match f()? {
            None => return Ok(()),
            Some(why) => anyhow::ensure!(Instant::now() < deadline, "waited {secs}s for {what}: {why}\n--- the server:\n{}", s.log()),
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Whether runner `name` is connected.
fn connected(s: &Server, name: &str) -> anyhow::Result<bool> {
    let (_, body) = http(s.port, "GET", "/api/agents", &s.admin, None)?;
    let v: Value = serde_json::from_str(&body).unwrap_or_default();
    let list = v.as_array().cloned().or_else(|| v.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
    Ok(list.iter().any(|a| a.get("name").and_then(Value::as_str) == Some(name) && a.get("connected").and_then(Value::as_bool) == Some(true)))
}

/// A series' value in the server's metrics (0 when it has none).
fn metric(s: &Server, series: &str) -> u64 {
    let (_, text) = http(s.port, "GET", "/metrics", &s.admin, None).unwrap_or_default();
    text.lines().find_map(|l| l.strip_prefix(series).and_then(|v| v.trim().parse().ok())).unwrap_or(0)
}

/// The kept workspaces on the keeper.
fn workspaces(work: &Path) -> usize {
    std::fs::read_dir(work.join("w")).map(|d| d.flatten().count()).unwrap_or(0)
}

#[test]
fn a_runners_commands_are_answered_through_lost_frames_and_a_cut_link() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::write(repo.join("README.md"), "commands\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!("[[repos]]\nname = \"acme/commands\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    // About one in two of the server's channel frames to its runners (`ServerMsg::Wire`, variant
    // 54) never reach the wire.
    let s = start_env(dir, &Options { agent: false, ci: "kiln_workflows = true", extra: &extra }, false, &[("KILN_CHAOS", "shed=control/54:2")])?;
    let join = std::fs::read_to_string(dir.join("data/join.token"))?.trim().to_owned();
    let work = dir.join("keeper-work");
    std::fs::create_dir_all(dir.join("keeper-home"))?;
    let out = std::fs::File::create(dir.join("keeper.log"))?;
    let _keeper = Keeper(
        Command::new(env!("CARGO_BIN_EXE_kiln"))
            .args(["agent", "--server", &format!("http://127.0.0.1:{}", s.port), "--token", &join, "--name", "keeper", "--labels", "keep", "--slots", "1", "--no-terminal", "--chaos", "drop=60s,down=15s"])
            .arg("--work-dir")
            .arg(&work)
            .arg("--state-dir")
            .arg(dir.join("keeper-state"))
            .env("HOME", dir.join("keeper-home"))
            .env("NO_COLOR", "1")
            .env_remove("KILN_CONFIG")
            .env_remove("KILN_CHAOS")
            .env_remove("KILN_REACH_SOCK")
            .stdout(Stdio::from(out.try_clone()?))
            .stderr(Stdio::from(out))
            .spawn()?,
    );
    let keeper_log = || std::fs::read_to_string(dir.join("keeper.log")).unwrap_or_default();
    until(&s, 120, "the repository polled and the keeper connected", || {
        let (_, repos) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok((repos.contains("\"last_polled_at\":null") || !connected(&s, "keeper")?).then(|| format!("{repos}\n{}", keeper_log())))
    })?;
    // A leg that fails until a file is there, kept when it fails.
    let marker = dir.join("fixed");
    let workflow = format!(
        "on: push\njobs:\n  kept:\n    runs-on: keep\n    checkout: false\n    debug: {{ keep-on-failure: 10m }}\n    steps:\n      - shell: sh\n        run: test -f \"{}\" && echo fixed-now\n",
        marker.display()
    );
    std::fs::write(repo.join(".kiln/kept.yaml"), workflow)?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "kept"])?;
    let mut leg = String::new();
    until(&s, 300, "the failed leg, kept", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/commands&limit=5", &s.admin, None)?;
        let runs: Value = serde_json::from_str(&body).unwrap_or_default();
        let Some(run) = runs.pointer("/runs/0/id").and_then(Value::as_str) else { return Ok(Some(body)) };
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
        let legs = serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default();
        leg = legs.first().and_then(|l| l.get("id")).and_then(Value::as_str).unwrap_or_default().to_owned();
        let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
        let about: Value = serde_json::from_str(&about).unwrap_or_default();
        let kept = about.pointer("/leg/kept_until").is_some_and(|k| !k.is_null());
        Ok((!kept || about.pointer("/leg/conclusion") != Some(&json!("failure"))).then(|| about.to_string()))
    })?;
    // A live command answered through the lost frames: the step rerun in place passes. (A cut can
    // fall on it: refused at once then, and asked again.)
    std::fs::write(&marker, "")?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (st, body) = http(s.port, "POST", &format!("/api/ci/legs/{leg}/rerun-step"), &s.admin, Some(&json!({ "idx": 0 })))?;
        if (200..300).contains(&st) {
            break;
        }
        anyhow::ensure!(st == 503 && Instant::now() < deadline, "the rerun wasn't taken: {st} {body}\n{}", s.log());
        std::thread::sleep(Duration::from_secs(1));
    }
    until(&s, 120, "the rerun to pass", || {
        let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
        let about: Value = serde_json::from_str(&about).unwrap_or_default();
        let passed = about.get("steps").and_then(Value::as_array).into_iter().flatten().any(|x| x.get("idx") != Some(&json!(0)) && x.get("conclusion") == Some(&json!("success")));
        Ok((!passed).then(|| about.to_string()))
    })?;
    let shed = metric(&s, r#"kiln_net_frames_dropped_total{lane="control",reason="chaos"}"#);
    assert!(shed > 0, "the server's frames to its runner were lost below the channel, and the commands answered all the same");
    // The keeper away: a live command is refused at once, a parked one answered parked.
    let kept = workspaces(&work);
    assert!(kept > 0, "the kept leg's workspace is there");
    until(&s, 120, "the keeper's link cut", || Ok(connected(&s, "keeper")?.then(|| "still connected".to_owned())))?;
    let (st, body) = http(s.port, "POST", &format!("/api/ci/legs/{leg}/rerun-step"), &s.admin, Some(&json!({ "idx": 0 })))?;
    assert_eq!(st, 503, "a rerun while its runner is away is refused at once: {body}");
    let t = Instant::now();
    let (st, body) = http(s.port, "POST", &format!("/api/ci/legs/{leg}/release"), &s.admin, Some(&json!({})))?;
    assert!((200..300).contains(&st) && t.elapsed() < Duration::from_secs(10), "a release while its runner is away waits for it, not its caller: {st} {body}");
    // Back: the release reaches it, once, and the workspace goes.
    until(&s, 120, "the release to reach the keeper", || Ok((workspaces(&work) >= kept).then(|| format!("{} workspaces\n{}", workspaces(&work), keeper_log()))))?;
    assert!(connected(&s, "keeper")?, "it came back");
    Ok(())
}
