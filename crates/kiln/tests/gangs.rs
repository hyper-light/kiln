//! W05-R40 and W05-AC18 (docs/ci.md, docs/needs.md E4.4): a job's `together: true` makes its legs,
//! its matrix, one gang, which starts whole or not at all. With three runner processes of the
//! test's own, one slot each: a gang of three queued while one runner is full starts once it frees,
//! all three within a second of each other. Built with `chaos`: a member refused by its runner's own
//! admission has the other two stopped within a second, and the three then run together, the legs'
//! records saying so; and a gang whose grants the store can't write starts none of its members, then
//! all three together.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
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
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A server without a runner of its own, whose grants' runners stop them within 5 s of being told
/// to (the lock-delay), `env` its faults.
fn server(dir: &Path, env: &[(&str, &str)]) -> anyhow::Result<Server> {
    start_env(dir, &Options { agent: false, ci: "kiln_workflows = true", extra: "[sched]\nlock_delay = \"5s\"\n" }, false, env)
}

/// `s`'s repository `acme/gang`, made and pushed a first commit, then `workflow`.
fn push_to(s: &Server, dir: &Path, workflow: &str) -> anyhow::Result<()> {
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/gang", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.join("githome"), dir.join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/gang.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "gang\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::write(work.join(".kiln/gang.yaml"), workflow)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "a gang"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    Ok(())
}

/// A runner process of the test's own (`kiln agent`), its output in its log: killed when dropped.
struct Runner {
    child: std::process::Child,
}

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runner `name` of `s`, labelled `local`, one slot, its home and folders under `dir` (`env` its
/// faults), once the server has it connected.
fn runner(s: &Server, dir: &Path, name: &str, env: &[(&str, &str)]) -> anyhow::Result<Runner> {
    let join = std::fs::read_to_string(dir.join("data/join.token"))?.trim().to_owned();
    let home = dir.join(format!("{name}-home"));
    std::fs::create_dir_all(&home)?;
    let log = dir.join(format!("{name}.log"));
    let out = std::fs::File::create(&log)?;
    let child = common::kiln()
        .args(["agent", "--server", &format!("http://127.0.0.1:{}", s.port), "--token", &join, "--name", name, "--labels", "local", "--slots", "1", "--no-terminal"])
        .arg("--work-dir")
        .arg(dir.join(format!("{name}-work")))
        .arg("--state-dir")
        .arg(dir.join(format!("{name}-state")))
        .env("HOME", &home)
        // A machine of its own (docs/machines.md): a test's runners would all be this one.
        .env("KILN_MACHINE_ID", name)
        .env("NO_COLOR", "1")
        .env_remove("KILN_CONFIG")
        .env_remove("KILN_CHAOS")
        .envs(env.iter().copied())
        .stdout(std::process::Stdio::from(out.try_clone()?))
        .stderr(std::process::Stdio::from(out))
        .spawn()?;
    let r = Runner { child };
    until(s, 120, &format!("runner {name} to connect"), || {
        let (_, body) = http(s.port, "GET", "/api/agents", &s.admin, None)?;
        let agents: Value = serde_json::from_str(&body).unwrap_or_default();
        let list = agents.as_array().cloned().or_else(|| agents.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
        // Known by its machine id (docs/machines.md), as main's tests find theirs.
        let up = list.iter().any(|a| a.get("name").and_then(Value::as_str) == Some(common::machine(name).as_str()) && a.get("connected").and_then(Value::as_bool).unwrap_or(true));
        Ok((!up).then(|| format!("{body}\n{}", std::fs::read_to_string(&log).unwrap_or_default())))
    })?;
    Ok(r)
}

/// Three runners of one slot, `r1` to `r3`, `r2`'s faults `faulty`.
fn three(s: &Server, dir: &Path, faulty: &[(&str, &str)]) -> anyhow::Result<[Runner; 3]> {
    Ok([runner(s, dir, "r1", &[])?, runner(s, dir, "r2", faulty)?, runner(s, dir, "r3", &[])?])
}

/// `s`'s run once it ended, within `secs` seconds; and the legs of its job `sim`, each as the
/// server has it (`GET /api/ci/legs/:id`: its attempts and annotations).
fn ended(s: &Server, secs: u64) -> anyhow::Result<(Value, Vec<Value>)> {
    let mut run = Value::Null;
    until(s, secs, "the run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/gang&limit=5", &s.admin, None)?;
        run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0").cloned().unwrap_or_default();
        Ok(run.get("finished_at").is_none_or(Value::is_null).then_some(body))
    })?;
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let mut legs = Vec::new();
    for l in serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).into_iter().flatten() {
        if l.get("key").and_then(Value::as_str).is_some_and(|k| k.starts_with("sim")) {
            let leg = l.get("id").and_then(Value::as_str).unwrap_or_default();
            legs.push(serde_json::from_str(&http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?.1)?);
        }
    }
    Ok((run, legs))
}

/// The time now, in seconds since the epoch, as a step's `perl` line writes it.
#[cfg(feature = "chaos")]
fn now() -> anyhow::Result<f64> {
    Ok(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs_f64())
}

/// The times written into each of `dir`'s files whose names begin with `prefix`, by name.
fn ticks(dir: &Path, prefix: &str) -> anyhow::Result<Vec<(String, Vec<f64>)>> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let name = e?.file_name().to_string_lossy().into_owned();
        if name.starts_with(prefix) {
            let times = std::fs::read_to_string(dir.join(&name))?.lines().filter_map(|l| l.trim().parse().ok()).collect();
            v.push((name, times));
        }
    }
    v.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(v)
}

/// The spread of the gang's starts: the latest first time of `files` less the earliest.
fn spread(files: &[(String, Vec<f64>)]) -> f64 {
    let firsts: Vec<f64> = files.iter().filter_map(|(_, t)| t.first().copied()).collect();
    firsts.iter().copied().fold(f64::MIN, f64::max) - firsts.iter().copied().fold(f64::MAX, f64::min)
}

/// A shell line writing the time now, to the millisecond, into `$f` (`perl`, as macOS's `date`
/// has no fractions).
const STAMP: &str = r#"perl -MTime::HiRes=time -e 'printf "%.3f\n", time' >> "$f""#;

/// Each leg of the gang ran once, its one attempt a success.
fn once_each(legs: &[Value], s: &Server) {
    assert_eq!(legs.len(), 3, "three legs: {legs:?}");
    for l in legs {
        let attempts = l.get("attempts").and_then(Value::as_array).cloned().unwrap_or_default();
        assert_eq!(attempts.len(), 1, "one attempt: {attempts:?}\n{}", s.log());
        assert_eq!(attempts.first().and_then(|a| a.get("conclusion")), Some(&json!("success")), "{attempts:?}");
    }
}

/// W05-AC18, the example of docs/needs.md E4.4: three runners of one slot, one of them full for its
/// first 20 s (job `busy`), and a matrix of three with `together: true` (job `sim`) queued while it
/// is. No member starts before the runner frees, and the three start within a second of each other.
#[test]
fn w05_ac18_a_gang_of_three_starts_whole() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let marks = dir.path().join("marks");
    std::fs::create_dir_all(&marks)?;
    let s = server(dir.path(), &[])?;
    let _runners = three(&s, dir.path(), &[])?;
    let workflow = r#"on: push
jobs:
  busy:
    runs-on: local
    steps:
      - run: |
          f="@M@/busy.started"; STAMP
          sleep 20
          f="@M@/busy.ended"; STAMP
  go:
    runs-on: local
    steps:
      - run: until [ -e "@M@/busy.started" ]; do sleep 0.2; done
  sim:
    runs-on: local
    needs: go
    strategy:
      matrix:
        node: [a, b, c]
    together: true
    steps:
      - run: |
          f="@M@/sim-${{ matrix.node }}"; STAMP
          sleep 2
"#
    .replace("@M@", &marks.display().to_string())
    .replace("STAMP", STAMP);
    push_to(&s, dir.path(), &workflow)?;
    let (run, legs) = ended(&s, 240)?;
    assert_eq!(run.get("conclusion"), Some(&json!("success")), "{run}\n{}", s.log());
    let freed = ticks(&marks, "busy.ended")?.first().and_then(|(_, t)| t.first().copied()).unwrap_or(f64::MAX);
    let sim = ticks(&marks, "sim-")?;
    assert_eq!(sim.len(), 3, "{sim:?}");
    for (name, t) in &sim {
        assert!(t.first().is_some_and(|start| *start >= freed), "{name} started at {t:?}, before the full runner freed at {freed}");
    }
    let apart = spread(&sim);
    eprintln!("w05_ac18: the gang's three started within {apart:.3} s of each other, after the runner freed at {freed:.3}");
    assert!(apart <= 1.0, "the three start within a second of each other: {sim:?}");
    once_each(&legs, &s);
    Ok(())
}

/// W05-AC18, the example of docs/needs.md E4.4: a gang of three on three runners of one slot, and
/// runner `r2`'s own admission refuses its member (`refuse=sim`), its refusal reaching the server
/// 3 s late (`delay=3s`), so the other two are running by then. They're stopped within a second of
/// the refusal; once their runners have stopped them (the lock-delay, 5 s here) the three run
/// together; and each leg's records say so: the refused member's that its runner refused it, the
/// others' that they were stopped, and each one attempt, a success, the one that ran whole.
#[cfg(feature = "chaos")]
#[test]
fn w05_ac18_a_refused_member_takes_its_gang_back() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let marks = dir.path().join("marks");
    std::fs::create_dir_all(&marks)?;
    let s = server(dir.path(), &[])?;
    let _runners = three(&s, dir.path(), &[("KILN_CHAOS", "refuse=sim,delay=3s")])?;
    // A first round ticks until it's stopped; once the test says `again`, a round ticks 2 s.
    let workflow = r#"on: push
jobs:
  sim:
    runs-on: local
    strategy:
      matrix:
        node: [a, b, c]
    together: true
    steps:
      - run: |
          r=1; n=600
          if [ -e "@M@/again" ]; then r=2; n=10; fi
          f="@M@/r$r-${{ matrix.node }}.$$"
          i=0
          while [ $i -lt $n ]; do STAMP; sleep 0.2; i=$((i+1)); done
"#
    .replace("@M@", &marks.display().to_string())
    .replace("STAMP", STAMP);
    push_to(&s, dir.path(), &workflow)?;
    until(&s, 180, "a member refused", || Ok((!s.log().contains("a member of its gang was refused: its gang goes again whole")).then(|| format!("{:?}", ticks(&marks, "r").unwrap_or_default()))))?;
    let refused = now()?;
    std::fs::write(marks.join("again"), "")?;
    until(&s, 30, "the other two stopped", || Ok((s.log().matches("this one is stopped, and its gang goes again whole").count() < 2).then(|| s.log().lines().filter(|l| l.contains("gang")).collect::<Vec<_>>().join("\n"))))?;
    std::thread::sleep(Duration::from_secs(2));
    let first = ticks(&marks, "r1-")?;
    assert_eq!(first.len(), 2, "the other two ran before the refusal reached the server: {first:?}\n{}", s.log());
    for (name, t) in &first {
        let last = t.last().copied().unwrap_or(f64::MAX);
        assert!(last <= refused + 1.0, "{name} stopped within a second of the refusal at {refused:.3}: its last tick {last:.3}");
    }
    eprintln!("w05_ac18: the other two stopped {:.3} s after the test saw the refusal at the latest", first.iter().filter_map(|(_, t)| t.last()).fold(f64::MIN, |a, b| a.max(*b)) - refused);
    let (run, legs) = ended(&s, 180)?;
    assert_eq!(run.get("conclusion"), Some(&json!("success")), "{run}\n{}", s.log());
    let whole = ticks(&marks, "r2-")?;
    assert_eq!(whole.len(), 3, "the three ran together: {whole:?}");
    assert!(spread(&whole) <= 1.0, "the three start within a second of each other: {whole:?}");
    once_each(&legs, &s);
    // Each leg's records: the attempt that ran whole, and the refused one's notes.
    let notes: Vec<String> = legs.iter().flat_map(|l| l.get("annotations").and_then(Value::as_array).cloned().unwrap_or_default()).filter(|a| a.get("title") == Some(&json!("gang"))).filter_map(|a| a.get("message").and_then(Value::as_str).map(str::to_owned)).collect();
    assert_eq!(notes.iter().filter(|m| m.starts_with("Refused by r2: its gang goes again whole")).count(), 1, "{notes:?}");
    assert_eq!(notes.iter().filter(|m| m.starts_with("Stopped on r1: ") || m.starts_with("Stopped on r3: ")).count(), 2, "{notes:?}");
    Ok(())
}

/// W05-R40, W05-R05, docs/needs.md E4.4: the store can't write a gang's grants (`claim=3s`: held
/// back 3 s, then failed). No runner hears of any of the three: none starts while the write is held
/// or after it fails; all three are queued again, and start together a while later, each on its one
/// attempt.
#[cfg(feature = "chaos")]
#[test]
fn w05_r40_no_runner_hears_of_a_gang_whose_grants_can_t_be_written() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let marks = dir.path().join("marks");
    std::fs::create_dir_all(&marks)?;
    let s = server(dir.path(), &[("KILN_CHAOS", "claim=3s")])?;
    let _runners = three(&s, dir.path(), &[])?;
    let workflow = r#"on: push
jobs:
  sim:
    runs-on: local
    strategy:
      matrix:
        node: [a, b, c]
    together: true
    steps:
      - run: |
          f="@M@/sim-${{ matrix.node }}"; STAMP
"#
    .replace("@M@", &marks.display().to_string())
    .replace("STAMP", STAMP);
    push_to(&s, dir.path(), &workflow)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while !s.log().contains("a gang's grants couldn't be written") {
        anyhow::ensure!(ticks(&marks, "sim-")?.is_empty(), "a member started while its gang's grants were held back\n{}", s.log());
        anyhow::ensure!(Instant::now() < deadline, "the gang's write never failed\n{}", s.log());
        std::thread::sleep(Duration::from_millis(100));
    }
    let failed = now()?;
    anyhow::ensure!(ticks(&marks, "sim-")?.is_empty(), "a member started under grants that weren't written\n{}", s.log());
    let (run, legs) = ended(&s, 180)?;
    assert_eq!(run.get("conclusion"), Some(&json!("success")), "{run}\n{}", s.log());
    let sim = ticks(&marks, "sim-")?;
    assert_eq!(sim.len(), 3, "{sim:?}");
    for (name, t) in &sim {
        assert!(t.first().is_some_and(|start| *start >= failed + 4.0), "{name} queued again, not started at once: {t:?}, the write failed at {failed:.3}");
    }
    assert!(spread(&sim) <= 1.0, "the three start within a second of each other: {sim:?}");
    once_each(&legs, &s);
    Ok(())
}
