//! W05-AC09 (docs/ci.md): the server restarted under a queue. Its unfinished run comes back from
//! its plan's log: its queued legs queued again, its running legs re-adopted from the runner that
//! still runs them, which finishes them, and nothing assigned twice. `kiln ci bench` measures the
//! rebuild at 10,000 queued legs (`restart.rebuild_ms`, §1.5); here it's 1,000.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::{Child, Command, Stdio};
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

/// A runner of its own, apart from the server, so it outlives the server's restart.
struct Runner(Child);

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// How many queued legs the run has besides its two slow ones.
const QUEUED: usize = 1_000;

/// The run's legs: `(id, key, status, attempt)` each.
fn legs(s: &Server, run: &str) -> anyhow::Result<Vec<(String, String, String, u64)>> {
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
    let detail: Value = serde_json::from_str(&detail)?;
    Ok(detail
        .get("legs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|l| {
            let text = |k: &str| l.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
            (text("id"), text("key"), text("status"), l.get("attempt").and_then(Value::as_u64).unwrap_or(0))
        })
        .collect())
}

/// W05-AC09: a run of two slow legs on a runner and 1,000 legs queued for runners there aren't,
/// the server killed and started again while the slow ones run. It rebuilds the run (all 1,002
/// legs, within 2 s), takes the slow legs back from the runner as it reconnects, which finishes
/// each once, and the queued ones wait on, none assigned.
#[test]
fn w05_ac09_a_restart_rebuilds_the_queue() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut s = start(dir.path(), &Options { agent: false, ci: "kiln_workflows = true", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/restart", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let join = std::fs::read_to_string(dir.path().join("data/join.token"))?.trim().to_owned();
    let home = dir.path().join("runner-home");
    std::fs::create_dir_all(&home)?;
    let log = std::fs::File::create(dir.path().join("runner.log"))?;
    let child = Command::new(env!("CARGO_BIN_EXE_kiln"))
        .args(["agent", "--server", &format!("http://127.0.0.1:{}", s.port), "--token", &join, "--name", "keeper", "--labels", "keep", "--slots", "2", "--no-terminal"])
        .arg("--work-dir")
        .arg(dir.path().join("runner-work"))
        .arg("--state-dir")
        .arg(dir.path().join("runner-state"))
        .env("HOME", &home)
        .env("NO_COLOR", "1")
        .env_remove("KILN_CONFIG")
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?;
    let _runner = Runner(child);
    let runner_log = || std::fs::read_to_string(dir.path().join("runner.log")).unwrap_or_default();
    until(&s, 120, "the runner to connect", || {
        let (_, body) = http(s.port, "GET", "/api/agents", &s.admin, None)?;
        let agents: Value = serde_json::from_str(&body).unwrap_or_default();
        let list = agents.as_array().cloned().or_else(|| agents.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
        let up = list.iter().any(|a| a.get("name").and_then(Value::as_str) == Some("keeper") && a.get("connected").and_then(Value::as_bool).unwrap_or(true));
        Ok((!up).then(|| format!("{body}\n{}", runner_log())))
    })?;
    let (git_home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&git_home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/restart.git", s.admin, s.port);
    git(&git_home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "restart\n")?;
    git(&git_home, &work, &["add", "."])?;
    git(&git_home, &work, &["commit", "-qm", "first"])?;
    git(&git_home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    let axis = (0..QUEUED).map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
    let workflow = format!(
        "on: push\njobs:\n  slow:\n    runs-on: keep\n    matrix: {{ n: [1, 2] }}\n    steps:\n      - run: sleep 30; echo \"slow ${{{{ matrix.n }}}} done\"\n  queued:\n    runs-on: nowhere\n    matrix: {{ i: [{axis}] }}\n    steps:\n      - run: echo never\n"
    );
    std::fs::write(work.join(".kiln/queue.yaml"), workflow)?;
    git(&git_home, &work, &["add", "."])?;
    git(&git_home, &work, &["commit", "-qm", "the queue"])?;
    git(&git_home, &work, &["push", "-q", &push, "HEAD:main"])?;
    // The slow legs running, the rest queued.
    let mut run = String::new();
    until(&s, 300, "the slow legs running and the rest queued", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/restart&limit=5", &s.admin, None)?;
        run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        if run.is_empty() {
            return Ok(Some(body));
        }
        let all = legs(&s, &run)?;
        let running = all.iter().filter(|(_, k, st, _)| k.starts_with("slow") && st == "running").count();
        let queued = all.iter().filter(|(_, k, st, _)| k.starts_with("queued") && st == "queued").count();
        Ok((running < 2 || queued < QUEUED).then(|| format!("{running} running, {queued} queued")))
    })?;
    let slow: Vec<String> = legs(&s, &run)?.into_iter().filter(|(_, k, _, _)| k.starts_with("slow")).map(|(id, _, _, _)| id).collect();
    anyhow::ensure!(slow.len() == 2, "{slow:?}");
    // Past their checkout: their own step running.
    until(&s, 120, "the slow legs' steps running", || {
        for id in &slow {
            let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{id}"), &s.admin, None)?;
            let about: Value = serde_json::from_str(&about)?;
            let running = about
                .get("steps")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|x| x.get("name").and_then(Value::as_str).is_some_and(|n| n.starts_with("Run sleep")) && x.get("status").and_then(Value::as_str) == Some("running"));
            if !running {
                return Ok(Some(about.to_string()));
            }
        }
        Ok(None)
    })?;
    // Killed and started again while they run.
    s.restart()?;
    let mut rebuilt = String::new();
    until(&s, 60, "the run rebuilt", || {
        let text = std::fs::read_to_string(dir.path().join("server.log")).unwrap_or_default();
        rebuilt = text.lines().rev().find(|l| l.contains("the CI engine rebuilt its runs")).unwrap_or_default().to_owned();
        Ok(rebuilt.is_empty().then(|| "not yet".to_owned()))
    })?;
    let field = |name: &str| rebuilt.split_whitespace().find_map(|w| w.strip_prefix(&format!("{name}="))).and_then(|v| v.parse::<u64>().ok()).unwrap_or(u64::MAX);
    assert_eq!(field("legs"), u64::try_from(QUEUED + 2)?, "every leg rebuilt: {rebuilt}");
    assert!(field("ms") < 2_000, "rebuilt within 2 s: {rebuilt}");
    // The slow legs, taken back from the runner, finish there, each once.
    until(&s, 180, "the slow legs to finish", || {
        let all = legs(&s, &run)?;
        let done = all.iter().filter(|(_, k, st, _)| k.starts_with("slow") && st == "succeeded").count();
        let ended = all.iter().filter(|(_, k, st, _)| k.starts_with("slow") && st != "running" && st != "queued").count();
        if ended == 2 && done < 2 {
            let mut said = String::new();
            for id in &slow {
                let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{id}"), &s.admin, None)?;
                said.push_str(&format!("{about}\n"));
            }
            anyhow::bail!("the slow legs ended unfinished:\n{said}\n--- the runner:\n{}\n--- the server:\n{}", runner_log(), s.log());
        }
        Ok((done < 2).then(|| format!("{all:?}").chars().take(2_000).collect()))
    })?;
    let all = legs(&s, &run)?;
    for id in &slow {
        let leg = all.iter().find(|(i, _, _, _)| i == id);
        assert!(leg.is_some_and(|(_, _, st, attempt)| st == "succeeded" && *attempt == 1), "the same leg, its first attempt, finished: {leg:?}");
        let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{id}/logs?step=0"), &s.admin, None)?;
        assert!(text.contains("done"), "it ran to its end: {text}");
        let ran = runner_log().lines().filter(|l| l.contains("running leg") && l.contains(id.as_str())).count();
        assert_eq!(ran, 1, "assigned once: {}", runner_log());
    }
    // The queued ones wait on, none assigned.
    let queued = all.iter().filter(|(_, k, st, _)| k.starts_with("queued") && st == "queued").count();
    assert_eq!(queued, QUEUED, "the queue as it was");
    let (st, body) = http(s.port, "POST", &format!("/api/ci/runs/{run}/cancel"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!((200..300).contains(&st), "cancel: {st} {body}");
    Ok(())
}

/// A native repository's working tree, pushed to the server.
struct Tree {
    home: std::path::PathBuf,
    work: std::path::PathBuf,
    push: String,
}

impl Tree {
    /// `acme/restart`, made on `s` with a first commit, once the server has polled it.
    fn new(dir: &Path, s: &Server) -> anyhow::Result<Tree> {
        Tree::named(dir, s, "acme/restart")
    }

    /// `name`, made on `s` with a first commit, once the server has polled it.
    fn named(dir: &Path, s: &Server, name: &str) -> anyhow::Result<Tree> {
        let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": name, "mode": "native" })))?;
        anyhow::ensure!(st == 200, "{st} {body}");
        let key = name.replace('/', "_");
        let t = Tree { home: dir.join("githome"), work: dir.join(format!("tree-{key}")), push: format!("http://x:{}@127.0.0.1:{}/git/{name}.git", s.admin, s.port) };
        std::fs::create_dir_all(&t.home)?;
        std::fs::create_dir_all(t.work.join(".kiln"))?;
        git(&t.home, &t.work, &["init", "-q", "-b", "main"])?;
        t.commit("README.md", "restart\n")?;
        until(s, 60, "the first poll", || {
            let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
            Ok(body.contains("\"last_polled_at\":null").then_some(body))
        })?;
        Ok(t)
    }

    /// `path` written with `text`, committed and pushed.
    fn commit(&self, path: &str, text: &str) -> anyhow::Result<()> {
        std::fs::write(self.work.join(path), text)?;
        git(&self.home, &self.work, &["add", "."])?;
        git(&self.home, &self.work, &["commit", "-qm", path])?;
        git(&self.home, &self.work, &["push", "-q", &self.push, "HEAD:main"])?;
        Ok(())
    }
}

/// The newest run of `workflow`'s id, once there is one.
fn run_of(s: &Server, workflow: &str) -> anyhow::Result<String> {
    run_in(s, "acme/restart", workflow)
}

/// The newest run of `repo`'s `workflow`, once there is one.
fn run_in(s: &Server, repo: &str, workflow: &str) -> anyhow::Result<String> {
    let mut run = String::new();
    until(s, 120, "the run", || {
        let (_, body) = http(s.port, "GET", &format!("/api/ci/runs?repo={repo}&workflow={workflow}&limit=1"), &s.admin, None)?;
        run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok(run.is_empty().then_some(body))
    })?;
    Ok(run)
}

/// Each of `ids`' first attempt: its conclusion and failure class.
fn first_attempts(s: &Server, ids: &[String]) -> anyhow::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for id in ids {
        let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{id}"), &s.admin, None)?;
        let about: Value = serde_json::from_str(&about)?;
        let first = about.get("attempts").and_then(Value::as_array).and_then(|a| a.iter().find(|x| x.get("number").and_then(Value::as_i64) == Some(1))).cloned().unwrap_or_default();
        let text = |k: &str| first.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        out.push((text("conclusion"), text("class")));
    }
    Ok(out)
}

/// W05-R05 across a restart: the server killed while its own runner runs two legs, which go with
/// it, under a queue whose rebuild outlasts the runner's reconnecting (so it says hello first,
/// running nothing). The legs it ran were lost with it, not held: each attempt fails `runner-lost`
/// and the next runs on it, the only runner that can take them (W05-R16's default); then its
/// slots are free for the next leg.
#[test]
fn w05_r05_a_restart_loses_what_its_runner_lost() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let tree = Tree::new(dir.path(), &s)?;
    let axis = (0..QUEUED).map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
    tree.commit(
        ".kiln/queue.yaml",
        &format!("on: push\njobs:\n  slow:\n    runs-on: local\n    matrix: {{ n: [1, 2] }}\n    steps:\n      - run: if [ \"$KILN_ATTEMPT\" = 1 ]; then sleep 120; fi\n  queued:\n    runs-on: nowhere\n    matrix: {{ i: [{axis}] }}\n    steps:\n      - run: echo never\n"),
    )?;
    let run = run_of(&s, ".kiln/queue.yaml")?;
    let mut slow: Vec<String> = Vec::new();
    until(&s, 300, "the slow legs running", || {
        let all = legs(&s, &run)?;
        slow = all.iter().filter(|(_, k, st, _)| k.starts_with("slow") && st == "running").map(|(id, _, _, _)| id.clone()).collect();
        Ok((slow.len() < 2).then(|| format!("{:?}", all.iter().filter(|(_, k, _, _)| k.starts_with("slow")).collect::<Vec<_>>())))
    })?;
    // Killed and started again while they run: its runner, back with none of them, says so.
    s.restart()?;
    until(&s, 120, "the lost legs' next attempts", || {
        let all = legs(&s, &run)?;
        let on: Vec<_> = all.iter().filter(|(id, _, _, _)| slow.contains(id)).collect();
        Ok((on.len() < 2 || on.iter().any(|(_, _, st, attempt)| *attempt < 2 || st != "succeeded")).then(|| format!("{on:?}")))
    })?;
    for (conclusion, class) in first_attempts(&s, &slow)? {
        assert_eq!((conclusion.as_str(), class.as_str()), ("failure", "runner-lost"), "lost with the runner:\n{}", s.log());
    }
    // Its slots free: a leg for it now runs.
    tree.commit(".kiln/after.yaml", "on: push\njobs:\n  after:\n    runs-on: local\n    steps:\n      - run: echo after\n")?;
    let after = run_of(&s, ".kiln/after.yaml")?;
    until(&s, 120, "the next leg on the runner", || {
        let all = legs(&s, &after)?;
        Ok((!all.iter().any(|(_, _, st, _)| st == "succeeded")).then(|| format!("{all:?}")))
    })?;
    let (st, body) = http(s.port, "POST", &format!("/api/ci/runs/{run}/cancel"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!((200..300).contains(&st), "cancel: {st} {body}");
    Ok(())
}

/// W05-R25 across a restart: a run cancelled while its leg runs, the server killed before the
/// runner said the leg ended (the step is deaf to the cancel's signals). The rebuild cancels the
/// leg again, as the run's log says: lost with the runner, it ends cancelled instead of running
/// again, the run ends cancelled, and the runner's slot is free for the next leg.
#[test]
fn w05_r25_a_cancel_holds_over_a_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let tree = Tree::new(dir.path(), &s)?;
    tree.commit(".kiln/deaf.yaml", "on: push\njobs:\n  deaf:\n    runs-on: local\n    steps:\n      - run: trap '' INT TERM; sleep 300\n")?;
    let run = run_of(&s, ".kiln/deaf.yaml")?;
    until(&s, 300, "the leg running", || {
        let all = legs(&s, &run)?;
        Ok((!all.iter().any(|(_, _, st, _)| st == "running")).then(|| format!("{all:?}")))
    })?;
    let (st, body) = http(s.port, "POST", &format!("/api/ci/runs/{run}/cancel"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!((200..300).contains(&st), "cancel: {st} {body}");
    // The cancel in the run's log, its runner still in the leg's step: then the server goes.
    std::thread::sleep(Duration::from_secs(1));
    s.restart()?;
    until(&s, 120, "the run cancelled", || {
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
        let status = serde_json::from_str::<Value>(&detail)?.pointer("/run/status").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok((status != "cancelled").then(|| format!("{status}: {:?}", legs(&s, &run))))
    })?;
    let all = legs(&s, &run)?;
    assert!(all.iter().all(|(_, _, st, attempt)| st == "cancelled" && *attempt == 1), "the leg isn't run again: {all:?}\n{}", s.log());
    // Its slot free: a leg for it now runs.
    tree.commit(".kiln/next.yaml", "on: push\njobs:\n  next:\n    runs-on: local\n    steps:\n      - run: echo next\n")?;
    let next = run_of(&s, ".kiln/next.yaml")?;
    until(&s, 120, "the next leg on the runner", || {
        let all = legs(&s, &next)?;
        Ok((!all.iter().any(|(_, _, st, _)| st == "succeeded")).then(|| format!("{all:?}")))
    })?;
    Ok(())
}

/// A repository deleted while its legs run takes them with it, as a deleted GitHub repository
/// does: its runner stops them, and a slot they held goes to the next repository's leg.
#[test]
fn a_deleted_repository_takes_its_legs_with_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let held = Tree::new(dir.path(), &s)?;
    held.commit(".kiln/hold.yaml", "on: push\njobs:\n  hold:\n    runs-on: local\n    matrix: { n: [1, 2, 3] }\n    steps:\n      - run: sleep 300\n")?;
    let run = run_of(&s, ".kiln/hold.yaml")?;
    until(&s, 300, "both of the runner's slots held", || {
        let all = legs(&s, &run)?;
        Ok((all.iter().filter(|(_, _, st, _)| st == "running").count() < 2).then(|| format!("{all:?}")))
    })?;
    // Another repository's leg waits for a slot.
    let other = Tree::named(dir.path(), &s, "acme/other")?;
    other.commit(".kiln/next.yaml", "on: push\njobs:\n  next:\n    runs-on: local\n    steps:\n      - run: echo next\n")?;
    let next = run_in(&s, "acme/other", ".kiln/next.yaml")?;
    let (_, repos) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
    let id = serde_json::from_str::<Value>(&repos)?
        .as_array()
        .and_then(|r| r.iter().find(|x| x.get("name").and_then(Value::as_str) == Some("acme/restart")))
        .and_then(|x| x.get("id").and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned();
    let (st, body) = http(s.port, "DELETE", &format!("/api/repos/{id}"), &s.admin, None)?;
    anyhow::ensure!((200..300).contains(&st), "delete: {st} {body}");
    until(&s, 120, "the next repository's leg, on a slot the deleted one's legs held", || {
        let all = legs(&s, &next)?;
        Ok((!all.iter().any(|(_, _, st, _)| st == "succeeded")).then(|| format!("{all:?}")))
    })?;
    Ok(())
}

/// The same with a runner that doesn't come back: the restart finds its leg running, and once
/// the runner has had its grace to reconnect (45 s), the leg was lost with it.
#[test]
fn w05_r05_a_runner_gone_over_a_restart_loses_its_legs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut s = start(dir.path(), &Options { agent: false, ci: "kiln_workflows = true", ..Options::default() })?;
    let join = std::fs::read_to_string(dir.path().join("data/join.token"))?.trim().to_owned();
    let home = dir.path().join("runner-home");
    std::fs::create_dir_all(&home)?;
    let log = std::fs::File::create(dir.path().join("runner.log"))?;
    let child = Command::new(env!("CARGO_BIN_EXE_kiln"))
        .args(["agent", "--server", &format!("http://127.0.0.1:{}", s.port), "--token", &join, "--name", "goner", "--labels", "gone", "--slots", "1", "--no-terminal"])
        .arg("--work-dir")
        .arg(dir.path().join("runner-work"))
        .arg("--state-dir")
        .arg(dir.path().join("runner-state"))
        .env("HOME", &home)
        .env("NO_COLOR", "1")
        .env_remove("KILN_CONFIG")
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?;
    let runner = Runner(child);
    let tree = Tree::new(dir.path(), &s)?;
    tree.commit(".kiln/gone.yaml", "on: push\njobs:\n  long:\n    runs-on: gone\n    steps:\n      - run: sleep 300\n")?;
    let run = run_of(&s, ".kiln/gone.yaml")?;
    let mut leg = Vec::new();
    until(&s, 300, "the leg running", || {
        let all = legs(&s, &run)?;
        leg = all.iter().filter(|(_, _, st, _)| st == "running").map(|(id, _, _, _)| id.clone()).collect();
        Ok(leg.is_empty().then(|| format!("{all:?}\n{}", std::fs::read_to_string(dir.path().join("runner.log")).unwrap_or_default())))
    })?;
    // The runner gone, then the server killed and started again.
    drop(runner);
    s.restart()?;
    until(&s, 150, "the leg lost with its runner", || {
        let all = legs(&s, &run)?;
        Ok((!all.iter().any(|(id, _, st, attempt)| leg.contains(id) && *attempt >= 2 && st != "running")).then(|| format!("{all:?}")))
    })?;
    for (conclusion, class) in first_attempts(&s, &leg)? {
        assert_eq!((conclusion.as_str(), class.as_str()), ("failure", "runner-lost"), "lost with the runner:\n{}", s.log());
    }
    let (st, body) = http(s.port, "POST", &format!("/api/ci/runs/{run}/cancel"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!((200..300).contains(&st), "cancel: {st} {body}");
    Ok(())
}
