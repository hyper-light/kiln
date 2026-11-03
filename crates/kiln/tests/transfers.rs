//! W27-R08 and W27-AC05 (docs/ci.md; the Transfers plan, kiln plan
//! `01a17856b1b477eda14a574ae312a123`): every kind of hand-over, each a transfer signed by both
//! sides. Agent sessions handed to a machine's newer process, moved between machines, resumed from
//! kiln's copy, carried through blue/green upgrades and the replant (the machine's runner role
//! with them); work handed between agents and to people; changes and review requests changing
//! hands; what one machine takes from another (a runner's objects from its peers). And the two
//! failures that started it played again, each refused and recorded: the 14:18 drop (a hand-over's
//! word for the runner being upgraded, passed on to its new build) and the twin (a copy left behind
//! acting as the session that went on elsewhere).
//!
//! `cargo test -p kiln --test transfers`; the blue/green hand-overs need a release build (an
//! upgrade downloads the server's own kiln, and a debug one is larger than kiln serves), and the
//! runners from before and the two failures a test build's faults: `cargo test --release -p kiln
//! --features chaos --test transfers` runs every one (.kiln/custody-lab.yaml, nightly).

#![cfg(unix)]

mod common;

use std::path::Path;
use std::time::Duration;

use common::agents::{self, Talk};
use common::custody::{Auth, ask, certified, field, half_named, person};
use common::{Options, http, machine, start};
use kiln_ci::identity::custody::{Body, Kind, to_value};
use kiln_ci::identity::statement::{Statement, Subject};
use serde_json::{Value, json};

const TALK: Talk = Talk { lines: 1, every_ms: 10 };

/// A newer runner process `id` of machine `name` (its first one's state folder, and its machine id,
/// as [`agents::runner`] gives it): a session is handed to it, an epoch up.
fn instance(dir: &Path, port: u16, name: &str, id: &str) -> anyhow::Result<agents::Runner> {
    instance_with(dir, port, name, id, None, &[])
}

/// [`instance`], with a test build's faults (`chaos`) and `env` in its environment.
fn instance_with(dir: &Path, port: u16, name: &str, id: &str, chaos: Option<&str>, env: &[(&str, &str)]) -> anyhow::Result<agents::Runner> {
    let join = std::fs::read_to_string(dir.join("data/join.token"))?.trim().to_owned();
    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("{name}-{id}.log")))?;
    let mut cmd = common::kiln();
    cmd.args(["agent", "--server", &format!("http://127.0.0.1:{port}"), "--token", &join, "--name", name, "--instance", id]);
    if let Some(c) = chaos {
        cmd.args(["--chaos", c]);
    }
    let child = cmd
        .arg("--work-dir")
        .arg(dir.join(format!("{name}-{id}-work")))
        .arg("--state-dir")
        .arg(dir.join(format!("{name}-state")))
        .env("HOME", dir.join("home"))
        .env("KILN_MACHINE_ID", name)
        .env("NO_COLOR", "1")
        .env("PATH", format!("{}:{}", dir.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
        .env("FAKE_CLAUDE_LINES", TALK.lines.to_string())
        .env("FAKE_CLAUDE_EVERY_MS", TALK.every_ms.to_string())
        .envs(env.iter().copied())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log))
        .spawn()?;
    Ok(agents::Runner(child))
}

/// The epoch session `id` was first certified at: once it is, the process holding it can sign it
/// on.
fn first_epoch(port: u16, admin: &str, id: &str) -> anyhow::Result<u64> {
    until(Duration::from_secs(60), || {
        let (st, body) = http(port, "GET", "/api/identities?kind=agent", admin, None)?;
        anyhow::ensure!(st == 200, "{st} {body}");
        let v: Value = serde_json::from_str(&body)?;
        let mark = format!("/agent/{id}/epoch/");
        Ok(v.get("identities").and_then(Value::as_array).and_then(|l| {
            l.iter().filter(|i| !field(i, "verified").is_empty()).find_map(|i| field(i, "subject").split_once(&mark).and_then(|(_, r)| r.split('/').next()).and_then(|e| e.parse().ok()))
        }))
    })
}

/// Once kiln keeps runner `name`'s copy of the stand-in harness's own record of a session
/// (Claude Code's, under `<dir>/home/.claude`), which a resume from kiln's copy carries: the record
/// written, then the runner's store said to be in step with the server's after its follower can
/// have found the record (`convo::record` looks for one every 15 s) and the runner said so (every
/// 5 s).
fn record_kept(dir: &Path, port: u16, admin: &str, name: &str) -> anyhow::Result<()> {
    record_written(dir)?;
    let after = kiln::util::now_ms() + 25_000;
    until(Duration::from_secs(120), || {
        let (st, body) = http(port, "GET", "/api/conversations/status", admin, None)?;
        anyhow::ensure!(st == 200, "{st} {body}");
        let v: Value = serde_json::from_str(&body)?;
        let stores = v.get("stores").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(stores.iter().any(|s| field(s, "machine") == machine(name) && field(s, "role") == "runner" && field(s, "state") == "in_sync" && s.get("heard").and_then(Value::as_i64).is_some_and(|h| h >= after)).then_some(()))
    })
}

/// Once the stand-in harness has written its own record of a session (Claude Code's, in a project
/// folder under `<dir>/home/.claude/projects`: its own, or the one a move installed it in).
fn record_written(dir: &Path) -> anyhow::Result<()> {
    let projects = dir.join("home").join(".claude").join("projects");
    let written = |p: &Path| std::fs::read_dir(p).ok().is_some_and(|d| d.flatten().any(|e| e.path().extension().is_some_and(|x| x == "jsonl") && e.metadata().is_ok_and(|m| m.len() > 0)));
    until(Duration::from_secs(60), || Ok(std::fs::read_dir(&projects).ok().and_then(|d| d.flatten().find(|e| written(&e.path()))).map(|_| ())))
}

/// Whether runner `name` is connected, as the server says (by its machine id).
fn connected(port: u16, admin: &str, name: &str) -> anyhow::Result<bool> {
    let (_, body) = http(port, "GET", "/api/agents", admin, None)?;
    let v: Value = serde_json::from_str(&body).unwrap_or_default();
    let list = v.as_array().cloned().or_else(|| v.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
    Ok(list.iter().any(|a| field(a, "name") == machine(name) && a.get("connected") == Some(&json!(true))))
}

/// The stand-in harnesses the sessions a test opens in `cwd` run in, stopped when it's dropped:
/// kiln keeps a harness running when its runner goes, as a real machine's does, so a test's
/// machine that goes away for good leaves its harness behind. Each says its pid in `cwd`'s
/// `.fake-claude-messages` as it's sent a message; only one whose home is the test's `home` is
/// stopped (its pid may be another process's since).
struct Harnesses {
    cwd: std::path::PathBuf,
    home: std::path::PathBuf,
}

impl Harnesses {
    fn new(cwd: &Path, home: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(cwd)?;
        std::fs::write(cwd.join(".fake-claude-messages"), "")?;
        Ok(Self { cwd: cwd.to_path_buf(), home: home.to_path_buf() })
    }
}

impl Drop for Harnesses {
    fn drop(&mut self) {
        let said = std::fs::read_to_string(self.cwd.join(".fake-claude-messages")).unwrap_or_default();
        let ours = format!("HOME={}", self.home.display());
        for pid in said.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()?.get("pid")?.as_i64()) {
            let env = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
            if env.split(|b| *b == 0).any(|v| v == ours.as_bytes())
                && let Ok(pid) = i32::try_from(pid)
            {
                let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
            }
        }
    }
}

/// Polls `f` every 250 ms for up to `within`, until it gives something.
fn until<T>(within: Duration, mut f: impl FnMut() -> anyhow::Result<Option<T>>) -> anyhow::Result<T> {
    let deadline = std::time::Instant::now() + within;
    loop {
        if let Some(v) = f()? {
            return Ok(v);
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "not within {within:?}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The session's chain of custody, by its timelines' name, once it walks unbroken through `hops`;
/// if it doesn't, what the server and the runner processes in `dir` logged about custody.
fn session_chain(dir: &Path, port: u16, admin: &str, id: &str, hops: usize) -> anyhow::Result<Value> {
    let chain = until(Duration::from_secs(120), || {
        let (st, body) = http(port, "GET", &format!("/api/custody?name=session%20{id}%20at%20epoch%20*"), admin, None)?;
        if st != 200 {
            return Ok(None);
        }
        let v: Value = serde_json::from_str(&body)?;
        let n = v.get("hops").and_then(Value::as_array).map_or(0, Vec::len);
        Ok((v.get("unbroken") == Some(&json!(true)) && n >= hops).then_some(v))
    });
    chain.map_err(|e| anyhow::anyhow!("{e}: {}", said_of(dir, port, admin, id)))
}

/// Session `id`'s chain as it stands, and what the server and the runner processes in `dir` logged
/// of it and of custody: why a test of it didn't go through.
fn said_of(dir: &Path, port: u16, admin: &str, id: &str) -> String {
    let (_, now) = http(port, "GET", &format!("/api/custody?name=session%20{id}%20at%20epoch%20*"), admin, None).unwrap_or_default();
    let mut names: Vec<std::path::PathBuf> = std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "log")).collect()).unwrap_or_default();
    names.sort();
    let logs: Vec<String> = names
        .iter()
        .map(|n| {
            let text = std::fs::read_to_string(n).unwrap_or_default();
            let lines: Vec<&str> = text.lines().filter(|l| l.contains(id) || ["custody", "took over", "taking over", "hand", "mov", "newest", "upgrad", "promot", "panicked", "fatal", "WARN", "ERROR"].iter().any(|w| l.contains(w))).collect();
            format!("{}:\n{}", n.display(), lines.get(lines.len().saturating_sub(60)..).unwrap_or(&[]).join("\n"))
        })
        .collect();
    format!("its chain now {now}\n{}", logs.join("\n"))
}

/// W27-R08 (the Transfers plan's step 3): an agent session handed to its machine's newer runner
/// process, twice, is signed for by both processes each time: the one it leaves signs its hand-on,
/// at the epoch it leaves; the one taking it signs its receipt, at the next. The chain walks
/// unbroken through both hand-overs, each hop naming the process that signed it.
#[test]
fn w27_r08_session_hand_overs_signed_by_both_processes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
    let (_main, log) = agents::runner(dir.path(), port, "box-a", None, TALK)?;
    agents::connected(port, admin, "box-a", &log, Duration::from_secs(60))?;
    let cwd = dir.path().join("work-here");
    std::fs::create_dir_all(&cwd)?;
    let id = agents::open_session(port, admin, "box-a", &cwd, Duration::from_secs(60))?;
    let e0 = first_epoch(port, admin, &id)?;
    let mut keep = Vec::new();
    keep.push(instance(dir.path(), port, "box-a", "z1")?);
    let one = session_chain(dir.path(), port, admin, &id, 2)?;
    let held = |v: &Value| -> Vec<(String, String)> {
        v.get("hops").and_then(Value::as_array).map(|h| h.iter().map(|h| (field(h, "kind").to_owned(), field(h, "custodian").to_owned())).collect()).unwrap_or_default()
    };
    let box_a = machine("box-a");
    let at = |e: u64, inst: &str| format!("/agent/{id}/epoch/{e}/machine/{box_a}/instance/{inst}");
    let first = held(&one);
    assert!(first.first().is_some_and(|(k, c)| k == "received" && c.ends_with(&at(e0 + 1, "z1"))), "{one}");
    assert!(first.get(1).is_some_and(|(k, c)| k == "handed-on" && c.ends_with(&at(e0, "main"))), "{one}");
    // Again, to a third process: the chain runs on through both hand-overs.
    keep.push(instance(dir.path(), port, "box-a", "z2")?);
    let two = session_chain(dir.path(), port, admin, &id, 4)?;
    let kinds: Vec<(String, String)> = held(&two);
    let expect = [("received", at(e0 + 2, "z2")), ("handed-on", at(e0 + 1, "z1")), ("received", at(e0 + 1, "z1")), ("handed-on", at(e0, "main"))];
    assert_eq!(kinds.len(), expect.len(), "{two}");
    for ((k, c), (want_k, want_c)) in kinds.iter().zip(expect.iter()) {
        assert!(k == want_k && c.ends_with(want_c.as_str()), "{k} {c}, wanted {want_k} {want_c}: {two}");
    }
    assert_eq!(half_named(&two), 0, "every hop names both its sides and who signed it");
    let (st, body) = http(port, "GET", "/api/custody/breaks", admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    assert_eq!(serde_json::from_str::<Value>(&body)?.get("breaks"), Some(&json!([])), "{body}");
    Ok(())
}

/// W27-R08 (the Transfers plan's step 3): a session whose machine is away, so can't hand it on,
/// goes on on another from kiln's copy as kiln's take: signed by kiln's server for the process
/// that held it, saying when kiln last heard from that machine, and received by the process it
/// goes to. The chain walks unbroken from that receipt to the take.
#[test]
fn w27_r08_a_session_resumed_from_kilns_copy_is_kilns_take() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
    // The stand-in keeps Claude Code's own record of the session, which a resume carries.
    let claude = dir.path().join("home").join(".claude").to_string_lossy().into_owned();
    let env = [("FAKE_CLAUDE_RECORD", "1"), ("CLAUDE_CONFIG_DIR", claude.as_str())];
    let (a, log_a) = agents::runner_with(dir.path(), port, "box-a", None, TALK, &env)?;
    let (_b, log_b) = agents::runner_with(dir.path(), port, "box-b", None, TALK, &env)?;
    agents::connected(port, admin, "box-a", &log_a, Duration::from_secs(60))?;
    agents::connected(port, admin, "box-b", &log_b, Duration::from_secs(60))?;
    let cwd = dir.path().join("work-here");
    std::fs::create_dir_all(&cwd)?;
    let id = agents::open_session(port, admin, "box-a", &cwd, Duration::from_secs(60))?;
    let e0 = first_epoch(port, admin, &id)?;
    record_kept(dir.path(), port, admin, "box-a")?;
    // box-a goes away: the session goes on on box-b, from kiln's copy.
    drop(a);
    until(Duration::from_secs(60), || Ok((!connected(port, admin, "box-a")?).then_some(())))?;
    let (st, body) = http(port, "POST", &format!("/api/harness/sessions/{id}/move"), admin, Some(&json!({"to": "box-b"})))?;
    anyhow::ensure!((200..300).contains(&st) && body.contains("from_store"), "{st} {body}");
    let v = session_chain(dir.path(), port, admin, &id, 2)?;
    let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
    let (got, took) = (hops.first().cloned().unwrap_or_default(), hops.get(1).cloned().unwrap_or_default());
    assert!(field(&got, "kind") == "received" && field(&got, "custodian").contains(&format!("/agent/{id}/epoch/{}/machine/{}/", e0 + 1, machine("box-b"))), "{v}");
    let server = format!("kiln://{}/server", field(&got, "custodian").trim_start_matches("kiln://").split('/').next().unwrap_or_default());
    assert!(field(&took, "kind") == "taken" && field(&took, "custodian") == server, "{v}");
    assert!(field(&took, "why").contains("box-a was last heard from at"), "{v}");
    assert_eq!(half_named(&v), 0, "every hop names both its sides and who signed it");
    Ok(())
}

/// W27-R08 (the Transfers plan's step 3): a session moved from one machine to another, both
/// online, is signed for by both processes: box-a's at the epoch it leaves, box-b's at the next.
#[test]
fn w27_r08_session_hand_overs_a_move_signed_by_both_machines() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
    // The stand-in keeps Claude Code's own record of the session, which a move carries.
    let claude = dir.path().join("home").join(".claude").to_string_lossy().into_owned();
    let env = [("FAKE_CLAUDE_RECORD", "1"), ("CLAUDE_CONFIG_DIR", claude.as_str())];
    let (_a, log_a) = agents::runner_with(dir.path(), port, "box-a", None, TALK, &env)?;
    let (_b, log_b) = agents::runner_with(dir.path(), port, "box-b", None, TALK, &env)?;
    agents::connected(port, admin, "box-a", &log_a, Duration::from_secs(60))?;
    agents::connected(port, admin, "box-b", &log_b, Duration::from_secs(60))?;
    let cwd = dir.path().join("work-here");
    std::fs::create_dir_all(&cwd)?;
    let id = agents::open_session(port, admin, "box-a", &cwd, Duration::from_secs(60))?;
    let e0 = first_epoch(port, admin, &id)?;
    record_written(dir.path())?;
    let moved = until(Duration::from_secs(60), || {
        let (st, body) = http(port, "POST", &format!("/api/harness/sessions/{id}/move"), admin, Some(&json!({"to": "box-b"})))?;
        Ok((200..300).contains(&st).then_some(body))
    })?;
    anyhow::ensure!(!moved.contains("from_store"), "a move between two machines online, not a resume: {moved}");
    let v = session_chain(dir.path(), port, admin, &id, 2)?;
    let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
    let (got, gave) = (hops.first().cloned().unwrap_or_default(), hops.get(1).cloned().unwrap_or_default());
    assert!(field(&got, "kind") == "received" && field(&got, "custodian").contains(&format!("/agent/{id}/epoch/{}/machine/{}/", e0 + 1, machine("box-b"))), "{v}");
    assert!(field(&gave, "kind") == "handed-on" && field(&gave, "custodian").ends_with(&format!("/agent/{id}/epoch/{e0}/machine/{}/instance/main", machine("box-a"))), "{v}");
    assert_eq!(half_named(&v), 0, "every hop names both its sides and who signed it");
    Ok(())
}

/// A runner kept by `kiln supervise`, as `kiln install --agent` runs one (so it upgrades
/// blue/green), in a process group of its own: dropped, the group goes, the supervisor with the
/// runner and the new build it started beside it.
struct Supervised(std::process::Child);

impl Drop for Supervised {
    fn drop(&mut self) {
        if let Ok(pid) = i32::try_from(self.0.id()) {
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Runner `name` under `kiln supervise`, as [`agents::runner`] starts one otherwise (a machine of its
/// own, by that name), writing to
/// `<dir>/<name>.log` (the new build an upgrade starts beside it too). It runs `exe`, a build other
/// than the server's, so the server has one to upgrade it to.
fn supervised(dir: &Path, port: u16, name: &str, exe: &Path, env: &[(&str, &str)]) -> anyhow::Result<(Supervised, std::path::PathBuf)> {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin)?;
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-claude/claude"), bin.join("claude"))?;
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755))?;
    let join = std::fs::read_to_string(dir.join("data/join.token"))?.trim().to_owned();
    let home = dir.join("home");
    std::fs::create_dir_all(&home)?;
    let log_path = dir.join(format!("{name}.log"));
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let mut cmd = common::kiln();
    cmd.arg("supervise")
        .arg("--exe")
        .arg(exe)
        .args(["--", "agent", "--server", &format!("http://127.0.0.1:{port}"), "--token", &join, "--name", name])
        .arg("--work-dir")
        .arg(dir.join(format!("{name}-work")))
        .arg("--state-dir")
        .arg(dir.join(format!("{name}-state")))
        .env("HOME", &home)
        .env_remove("GIT_CONFIG_GLOBAL")
        .env("KILN_MACHINE_ID", name)
        .env("NO_COLOR", "1")
        .env("FAKE_CLAUDE_LINES", TALK.lines.to_string())
        .env("FAKE_CLAUDE_EVERY_MS", TALK.every_ms.to_string())
        .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()))
        .envs(env.iter().copied())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log));
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    Ok((Supervised(cmd.spawn()?), log_path))
}

/// What the server took again from runner `name`'s store as it had it already, and what it left
/// out as a former writer's (rows sent twice, or lost to an epoch that rose too soon), since it
/// started: the board's feed intake.
fn sent_again(port: u16, admin: &str, name: &str) -> anyhow::Result<(u64, u64)> {
    let (st, body) = http(port, "GET", "/api/conversations/status", admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    let stores = v.get("stores").and_then(Value::as_array).cloned().unwrap_or_default();
    let n = |k: &str| stores.iter().filter(|s| field(s, "machine") == machine(name) && field(s, "role") == "runner").filter_map(|s| s.pointer(&format!("/feed/intake/{k}")).and_then(Value::as_u64)).sum();
    Ok((n("same"), n("fenced")))
}

/// W27-R08 (the Transfers plan's step 3): a blue/green upgrade hands its machine's session to the
/// new build signed for, an epoch up: the runner upgraded signs its hand-on at the epoch the
/// session had, the new build its receipt at the next once it's promoted, and it goes on at that
/// epoch, with nothing the server's copy has sent to it again.
#[test]
#[cfg_attr(debug_assertions, ignore = "needs a release build: an upgrade downloads the server's own kiln, and a debug one is over a gigabyte")]
fn w27_r08_session_hand_overs_blue_green_sends_nothing_again() -> anyhow::Result<()> {
    // The new build is this one, downloaded from the server: hundreds of MB for a debug build, so
    // under the build's own temporary folder, on disk.
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
    // box-a runs a build of its own (this one, a byte longer), so the server's is newer to it.
    let before_build = dir.path().join("kiln-before");
    let mut bytes = std::fs::read(env!("CARGO_BIN_EXE_kiln"))?;
    bytes.push(0);
    std::fs::write(&before_build, bytes)?;
    std::fs::set_permissions(&before_build, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    let (_box_a, log) = supervised(dir.path(), port, "box-a", &before_build, &[])?;
    agents::connected(port, admin, "box-a", &log, Duration::from_secs(60))?;
    let cwd = dir.path().join("work-here");
    std::fs::create_dir_all(&cwd)?;
    let id = agents::open_session(port, admin, "box-a", &cwd, Duration::from_secs(60))?;
    let e0 = first_epoch(port, admin, &id)?;
    let before = sent_again(port, admin, "box-a")?;
    let (st, body) = http(port, "POST", &format!("/api/updates/runners/{}", machine("box-a")), admin, None)?;
    anyhow::ensure!(st == 202, "{st} {body}");
    let job = serde_json::from_str::<Value>(&body)?.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    // What the upgrade, the runner and the server said of it, when it doesn't go through.
    let said = |e: anyhow::Error| {
        let (_, j) = http(port, "GET", &format!("/api/updates/jobs/{job}"), admin, None).unwrap_or_default();
        let lines = |f: &str| {
            let text = std::fs::read_to_string(dir.path().join(f)).unwrap_or_default();
            let hits: Vec<&str> = text.lines().filter(|l| ["upgrad", "build", "green", "hand", "custody", "promot", "take", "ready", "WARN", "ERROR"].iter().any(|w| l.contains(w))).collect();
            hits.get(hits.len().saturating_sub(60)..).unwrap_or(&[]).join("\n")
        };
        anyhow::anyhow!("{e}\njob: {j}\nbox-a:\n{}\nserver:\n{}", lines("box-a.log"), lines("server.log"))
    };
    let v = until(Duration::from_secs(300), || {
        let (st, body) = http(port, "GET", &format!("/api/custody?name=session%20{id}%20at%20epoch%20*"), admin, None)?;
        let v: Value = if st == 200 { serde_json::from_str(&body)? } else { Value::Null };
        let n = v.get("hops").and_then(Value::as_array).map_or(0, Vec::len);
        Ok((v.get("unbroken") == Some(&json!(true)) && n >= 2).then_some(v))
    })
    .map_err(said)?;
    let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
    let (got, gave) = (hops.first().cloned().unwrap_or_default(), hops.get(1).cloned().unwrap_or_default());
    let green = format!("/agent/{id}/epoch/{}/machine/{}/instance/", e0 + 1, machine("box-a"));
    assert!(field(&got, "kind") == "received" && field(&got, "custodian").contains(&green) && !field(&got, "custodian").ends_with("/instance/main"), "{v}");
    assert!(field(&gave, "kind") == "handed-on" && field(&gave, "custodian").ends_with(&format!("/agent/{id}/epoch/{e0}/machine/{}/instance/main", machine("box-a"))), "{v}");
    assert_eq!(half_named(&v), 0, "every hop names both its sides and who signed it");
    // It goes on at the next epoch, its state saying so, with nothing sent again.
    until(Duration::from_secs(60), || {
        let (_, body) = http(port, "GET", &format!("/api/harness/sessions/{id}"), admin, None)?;
        let i: Value = serde_json::from_str(&body).unwrap_or_default();
        let epoch = i.get("epoch").or_else(|| i.pointer("/info/epoch")).and_then(Value::as_u64);
        Ok((epoch == Some(e0 + 1)).then_some(()))
    })?;
    std::thread::sleep(Duration::from_secs(6));
    assert_eq!(sent_again(port, admin, "box-a")?, before, "rows the server had already, or left out as a former writer's, across the hand-over");
    Ok(())
}

/// Of session `id`'s chain of custody, the transfers: each receipt and the hand-on or take it
/// follows, both signed (the chain walks unbroken), as (epoch received at, its receiver).
fn transfers(v: &Value) -> Vec<(u64, String)> {
    let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
    let epoch = |c: &str| c.split_once("/epoch/").and_then(|(_, r)| r.split('/').next()).and_then(|e| e.parse().ok()).unwrap_or(u64::MAX);
    hops.windows(2)
        .filter(|w| matches!(w, [got, gave] if field(got, "kind") == "received" && matches!(field(gave, "kind"), "handed-on" | "taken") && !field(got, "signer").is_empty() && !field(gave, "signer").is_empty()))
        .filter_map(|w| w.first().map(|got| (epoch(field(got, "custodian")), field(got, "custodian").to_owned())))
        .collect()
}

/// W27-R08 (the Transfers plan's step 3, its test): one session moved to another machine, upgraded
/// there blue/green, taken over by a newer process of that machine, and resumed from kiln's copy
/// once that machine is away: four hand-overs, epochs e0 to e0 + 4, each a verified transfer.
#[test]
#[cfg_attr(debug_assertions, ignore = "needs a release build: an upgrade downloads the server's own kiln, and a debug one is over a gigabyte")]
fn w27_r08_session_hand_overs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
    let claude = dir.path().join("home").join(".claude").to_string_lossy().into_owned();
    let env = [("FAKE_CLAUDE_RECORD", "1"), ("CLAUDE_CONFIG_DIR", claude.as_str())];
    let (_a, log_a) = agents::runner_with(dir.path(), port, "box-a", None, TALK, &env)?;
    let before_build = dir.path().join("kiln-before");
    let mut bytes = std::fs::read(env!("CARGO_BIN_EXE_kiln"))?;
    bytes.push(0);
    std::fs::write(&before_build, bytes)?;
    std::fs::set_permissions(&before_build, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    let (box_b, log_b) = supervised(dir.path(), port, "box-b", &before_build, &env)?;
    agents::connected(port, admin, "box-a", &log_a, Duration::from_secs(60))?;
    agents::connected(port, admin, "box-b", &log_b, Duration::from_secs(60))?;
    let cwd = dir.path().join("work-here");
    std::fs::create_dir_all(&cwd)?;
    let id = agents::open_session(port, admin, "box-a", &cwd, Duration::from_secs(60))?;
    let e0 = first_epoch(port, admin, &id)?;
    let chain_through = |n: usize, after: &str| -> anyhow::Result<Value> {
        until(Duration::from_secs(300), || {
            let (st, body) = http(port, "GET", &format!("/api/custody?name=session%20{id}%20at%20epoch%20*"), admin, None)?;
            let v: Value = if st == 200 { serde_json::from_str(&body)? } else { Value::Null };
            Ok((v.get("unbroken") == Some(&json!(true)) && transfers(&v).len() >= n).then_some(v))
        })
        .map_err(|e| anyhow::anyhow!("{after}: {n} transfers {e}: {}", said_of(dir.path(), port, admin, &id)))
    };
    // Moved to box-b, both online.
    record_written(dir.path())?;
    until(Duration::from_secs(60), || {
        let (st, _) = http(port, "POST", &format!("/api/harness/sessions/{id}/move"), admin, Some(&json!({"to": "box-b"})))?;
        Ok((200..300).contains(&st).then_some(()))
    })?;
    chain_through(1, "moved")?;
    // box-b upgraded blue/green.
    let (st, body) = http(port, "POST", &format!("/api/updates/runners/{}", machine("box-b")), admin, None)?;
    anyhow::ensure!(st == 202, "{st} {body}");
    chain_through(2, "upgraded blue/green")?;
    // Taken over by a newer process of box-b: newer than the build the upgrade started, as its
    // instance record says (an instance started without one ranks oldest).
    let z1_dir = kiln::upgrade::instance_dir(&dir.path().join("box-b-state"), "z1");
    std::fs::create_dir_all(&z1_dir)?;
    std::fs::write(z1_dir.join("instance.json"), serde_json::to_vec(&kiln::upgrade::InstanceRecord { id: "z1".into(), created_at: kiln::util::now_unix() + 1 })?)?;
    let z1 = instance_with(dir.path(), port, "box-b", "z1", None, &env)?;
    chain_through(3, "taken over")?;
    // box-b away: it goes on on box-a, from kiln's copy.
    record_kept(dir.path(), port, admin, "box-b").map_err(|e| anyhow::anyhow!("box-b's copy of the record: {e}: {}", said_of(dir.path(), port, admin, &id)))?;
    drop((z1, box_b));
    until(Duration::from_secs(120), || Ok((!connected(port, admin, "box-b")?).then_some(()))).map_err(|e| {
        let (_, agents) = http(port, "GET", "/api/agents", admin, None).unwrap_or_default();
        anyhow::anyhow!("box-b away: {e}: {agents}")
    })?;
    let (st, body) = http(port, "POST", &format!("/api/harness/sessions/{id}/move"), admin, Some(&json!({"to": "box-a"})))?;
    anyhow::ensure!((200..300).contains(&st) && body.contains("from_store"), "{st} {body}");
    let v = chain_through(4, "resumed")?;
    let got = transfers(&v);
    assert_eq!(got.iter().map(|(e, _)| *e).collect::<Vec<_>>(), vec![e0 + 4, e0 + 3, e0 + 2, e0 + 1], "verified transfers, newest first: {v}");
    assert!(got.first().is_some_and(|(_, c)| c.contains(&format!("/machine/{}/", machine("box-a")))) && got.get(1).is_some_and(|(_, c)| c.ends_with("/instance/z1")), "{v}");
    assert_eq!(half_named(&v), 0, "every hop names both its sides and who signed it");
    Ok(())
}

/// W27-R08 (the Transfers plan's step 8): a replant moves box-a's runner, blue/green, onto the
/// server's build while it holds two agent sessions. The upgrade's record (its job) lists the
/// machine's runner role, handed on by the process replaced and received by its new build, and each
/// session, each a transfer signed by both sides; nothing moved without one.
#[test]
#[cfg_attr(debug_assertions, ignore = "needs a release build: an upgrade downloads the server's own kiln, and a debug one is over a gigabyte")]
fn w27_r08_replant() -> anyhow::Result<()> {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
    // box-a runs a build of its own (this one, a byte longer), so the server's is newer to it.
    let before_build = dir.path().join("kiln-before");
    let mut bytes = std::fs::read(env!("CARGO_BIN_EXE_kiln"))?;
    bytes.push(0);
    std::fs::write(&before_build, bytes)?;
    std::fs::set_permissions(&before_build, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    let (_box_a, log) = supervised(dir.path(), port, "box-a", &before_build, &[])?;
    agents::connected(port, admin, "box-a", &log, Duration::from_secs(60))?;
    let cwd = dir.path().join("work-here");
    std::fs::create_dir_all(&cwd)?;
    let ids = [agents::open_session(port, admin, "box-a", &cwd, Duration::from_secs(60))?, agents::open_session(port, admin, "box-a", &cwd, Duration::from_secs(60))?];
    // The replant: box-a moved onto the server's build.
    let (st, body) = http(port, "POST", &format!("/api/updates/runners/{}", machine("box-a")), admin, None)?;
    anyhow::ensure!(st == 202, "{st} {body}");
    let job = serde_json::from_str::<Value>(&body)?.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut seen = Value::Null;
    let v = until(Duration::from_secs(300), || {
        let (st, body) = http(port, "GET", &format!("/api/updates/jobs/{job}"), admin, None)?;
        let v: Value = if st == 200 { serde_json::from_str(&body)? } else { Value::Null };
        seen = v.clone();
        let n = v.get("transfers").and_then(Value::as_array).map_or(0, Vec::len);
        Ok((field(&v, "state") == "done" && n == 1 + ids.len()).then_some(v))
    })
    .map_err(|e| {
        let text = std::fs::read_to_string(dir.path().join("box-a.log")).unwrap_or_default();
        let hits: Vec<&str> = text.lines().filter(|l| ["upgrad", "green", "hand", "custody", "role", "promot", "WARN", "ERROR"].iter().any(|w| l.contains(w))).collect();
        anyhow::anyhow!("{e}\njob: {seen}\nbox-a:\n{}", hits.get(hits.len().saturating_sub(60)..).unwrap_or(&[]).join("\n"))
    })?;
    assert_eq!(v.get("moved_without_a_transfer"), Some(&json!([])), "{v}");
    let transfers = v.get("transfers").and_then(Value::as_array).cloned().unwrap_or_default();
    let of = |what: &str| transfers.iter().find(|t| field(t, "what") == what).cloned().unwrap_or_default();
    let box_a = machine("box-a");
    let role = of(&format!("runner role of {box_a}"));
    assert!(field(&role, "from").ends_with(&format!("/machine/{box_a}/instance/main")), "the process replaced hands the role on: {v}");
    assert!(field(&role, "to").contains(&format!("/machine/{box_a}/instance/")) && !field(&role, "to").ends_with("/instance/main"), "its new build holds it: {v}");
    for id in &ids {
        let t = of(&format!("session {id}"));
        assert!(field(&t, "from").contains(&format!("/agent/{id}/epoch/")) && field(&t, "from").ends_with(&format!("/machine/{box_a}/instance/main")), "{id} handed on by the process replaced: {v}");
        assert!(field(&t, "to").contains(&format!("/agent/{id}/epoch/")) && !field(&t, "to").ends_with("/instance/main"), "{id} received by its new build: {v}");
    }
    Ok(())
}

/// The hand-overs kiln recorded as unsigned, for session `id`, once there are `n`.
#[cfg(feature = "chaos")]
fn unsigned_of(dir: &Path, port: u16, admin: &str, id: &str, n: usize) -> anyhow::Result<Vec<Value>> {
    let mut seen = Value::Null;
    until(Duration::from_secs(120), || {
        let (st, body) = http(port, "GET", "/api/transfers/unsigned", admin, None)?;
        anyhow::ensure!(st == 200, "{st} {body}");
        let v: Value = serde_json::from_str(&body)?;
        let mine: Vec<Value> = v.get("unsigned").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter(|u| field(u, "session") == id).collect();
        seen = Value::Array(mine.clone());
        Ok((mine.len() >= n).then_some(mine))
    })
    .map_err(|e| anyhow::anyhow!("{n} unsigned hand-overs {e}: {seen}: {}", said_of(dir, port, admin, id)))
}

/// W27-R08 (the Transfers plan's step 3): a runner from before signed hand-overs (a test build's
/// `unsigned` fault) hands a session to a newer one, which hands it on to another from before:
/// both hand it over as they did, each hand-over recorded as unsigned, saying which side is from
/// before. `cargo test -p kiln --features chaos --test transfers`
#[cfg(feature = "chaos")]
#[test]
fn w27_r08_session_hand_overs_with_runners_from_before_go_unsigned() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
    let (_main, log) = agents::runner(dir.path(), port, "box-a", Some("unsigned"), TALK)?;
    agents::connected(port, admin, "box-a", &log, Duration::from_secs(60))?;
    let cwd = dir.path().join("work-here");
    std::fs::create_dir_all(&cwd)?;
    let id = agents::open_session(port, admin, "box-a", &cwd, Duration::from_secs(60))?;
    let e0 = first_epoch(port, admin, &id)?;
    // To a newer runner, from one from before: it signed no hand-on.
    let _z1 = instance(dir.path(), port, "box-a", "z1")?;
    let one = unsigned_of(dir.path(), port, admin, &id, 1)?;
    let first = one.iter().find(|u| u.get("epoch") == Some(&json!(e0 + 1))).cloned().unwrap_or_default();
    assert!(field(&first, "gave").ends_with(&format!("/agent/{id}/epoch/{e0}/machine/{}/instance/main", machine("box-a"))) && field(&first, "why").contains("signed no hand-on"), "{one:?}");
    // From the newer runner to another from before: it signed no receipt.
    let _z2 = instance_with(dir.path(), port, "box-a", "z2", Some("unsigned"), &[])?;
    let two = unsigned_of(dir.path(), port, admin, &id, 2)?;
    let second = two.iter().find(|u| u.get("epoch") == Some(&json!(e0 + 2))).cloned().unwrap_or_default();
    assert!(field(&second, "gave").ends_with(&format!("/agent/{id}/epoch/{}/machine/{}/instance/z1", e0 + 1, machine("box-a"))) && field(&second, "why").contains("signed no receipt"), "{two:?}");
    Ok(())
}

/// W27-AC05 (the Transfers plan's step 4): the 14:18 drop played again. Seven sessions on a machine
/// upgraded blue/green; as kiln certifies each at its next epoch for the new build, the server also
/// tells the runner it took them over from what it told it before transfers named a session's
/// holder (a test build's `stale_words`): hand each to the newest, and that it was taken. Every
/// such word is refused, and recorded naming whom it was for and the runner that refused it; none
/// is applied, and the new build holds all seven.
#[cfg(feature = "chaos")]
#[test]
#[cfg_attr(debug_assertions, ignore = "needs a release build: an upgrade downloads the server's own kiln, and a debug one is over a gigabyte")]
fn w27_ac05_the_1418_drop_played_again() -> anyhow::Result<()> {
    const SESSIONS: usize = 7;
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let s = common::start_env(dir.path(), &Options::default(), false, &[("KILN_CHAOS", "stale_words")])?;
    let (port, admin) = (s.port, s.admin.as_str());
    let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
    let before_build = dir.path().join("kiln-before");
    let mut bytes = std::fs::read(env!("CARGO_BIN_EXE_kiln"))?;
    bytes.push(0);
    std::fs::write(&before_build, bytes)?;
    std::fs::set_permissions(&before_build, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    let (_box_a, log) = supervised(dir.path(), port, "box-a", &before_build, &[])?;
    agents::connected(port, admin, "box-a", &log, Duration::from_secs(60))?;
    let cwd = dir.path().join("work-here");
    let mut sessions = Vec::new();
    for _ in 0..SESSIONS {
        let id = agents::open_session(port, admin, "box-a", &cwd, Duration::from_secs(60))?;
        let e0 = first_epoch(port, admin, &id)?;
        sessions.push((id, e0));
    }
    let (st, body) = http(port, "POST", &format!("/api/updates/runners/{}", machine("box-a")), admin, None)?;
    anyhow::ensure!(st == 202, "{st} {body}");
    // Each goes on at its next epoch in the new build, which received it.
    for (id, e0) in &sessions {
        let v = until(Duration::from_secs(300), || {
            let (st, body) = http(port, "GET", &format!("/api/custody?name=session%20{id}%20at%20epoch%20*"), admin, None)?;
            let v: Value = if st == 200 { serde_json::from_str(&body)? } else { Value::Null };
            Ok((v.get("unbroken") == Some(&json!(true)) && field(&v, "held_by").contains(&format!("/agent/{id}/epoch/{}/machine/{}/", e0 + 1, machine("box-a")))).then_some(v))
        })
        .map_err(|e| anyhow::anyhow!("session {id}: {e}: {}", said_of(dir.path(), port, admin, id)))?;
        assert!(!field(&v, "held_by").ends_with("/instance/main"), "held by the new build: {v}");
    }
    // The stale words: each refused, and recorded naming whom it was for and who refused it.
    let refused = until(Duration::from_secs(60), || {
        let (st, body) = http(port, "GET", "/api/transfers/refused", admin, None)?;
        anyhow::ensure!(st == 200, "{st} {body}");
        let v: Value = serde_json::from_str(&body)?;
        let words: Vec<Value> = v.get("refused").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter(|r| field(r, "act") == "a hand-over's word").collect();
        Ok((words.len() >= 2 * SESSIONS).then_some(words))
    })
    .map_err(|e| anyhow::anyhow!("{e}: {}", sessions.first().map(|(id, _)| said_of(dir.path(), port, admin, id)).unwrap_or_default()))?;
    for (id, _) in &sessions {
        let of: Vec<&Value> = refused.iter().filter(|r| field(r, "why").contains(id.as_str())).collect();
        assert_eq!(of.len(), 2, "both stale words about session {id} refused: {refused:?}");
        assert!(of.iter().all(|r| field(r, "came_from") == format!("runner {} (instance main)", machine("box-a")) && field(r, "why").contains("handed to its machine's new build")), "{of:?}");
    }
    // None applied: every session still runs, held by the new build.
    std::thread::sleep(Duration::from_secs(5));
    for (id, e0) in &sessions {
        let (st, body) = http(port, "GET", &format!("/api/harness/sessions/{id}"), admin, None)?;
        anyhow::ensure!(st == 200, "session {id}: {st} {body}");
        let i: Value = serde_json::from_str(&body).unwrap_or_default();
        let status = i.get("status").or_else(|| i.pointer("/info/status")).and_then(Value::as_str).unwrap_or_default();
        assert!(!matches!(status, "exited" | "closed"), "session {id} went on: {i}");
        let (_, chain) = http(port, "GET", &format!("/api/custody?name=session%20{id}%20at%20epoch%20*"), admin, None)?;
        let v: Value = serde_json::from_str(&chain).unwrap_or_default();
        assert!(field(&v, "held_by").contains(&format!("/agent/{id}/epoch/{}/machine/{}/", e0 + 1, machine("box-a"))) && !field(&v, "held_by").ends_with("/instance/main"), "{v}");
    }
    Ok(())
}

/// Operation `name` as an admin calls it (`POST /api/ops/<name>`): its answer.
#[cfg(feature = "chaos")]
fn op(port: u16, admin: &str, name: &str, input: &Value) -> anyhow::Result<Value> {
    let (st, body) = http(port, "POST", &format!("/api/ops/{name}"), admin, Some(input))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    Ok(serde_json::from_str(&body).unwrap_or(Value::Null))
}

/// W27-AC05 (the Transfers plan's step 5): the twin played again. A session moves off a machine
/// cut off from kiln (it goes on on another from kiln's copy); its copy there, still running at the
/// epoch it left, marks its goal met, starts a CI run and starts a run on another machine. None of
/// it is done: each act is refused, and recorded with who signed it; once the machine is back, its
/// runner is told to let go of the copy at its first word; and the session at its next epoch goes
/// on untouched.
#[cfg(feature = "chaos")]
#[test]
fn w27_ac05_the_twin_played_again() -> anyhow::Result<()> {
    use kiln::reach::local::{Client, Ev, Req};
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let cwd = dir.path().join("work-here");
    let _harnesses = Harnesses::new(&cwd, &dir.path().join("home"))?;
    let claude = dir.path().join("home").join(".claude").to_string_lossy().into_owned();
    let env = [("FAKE_CLAUDE_RECORD", "1"), ("CLAUDE_CONFIG_DIR", claude.as_str())];
    // box-a's link drops a minute after each connection, and stays away a minute.
    let (_a, log_a) = agents::runner_with(dir.path(), port, "box-a", Some("drop=60s,down=60s"), TALK, &env)?;
    let (_b, log_b) = agents::runner_with(dir.path(), port, "box-b", None, TALK, &env)?;
    let (_c, log_c) = agents::runner_with(dir.path(), port, "box-c", None, TALK, &env)?;
    for (name, log) in [("box-a", &log_a), ("box-b", &log_b), ("box-c", &log_c)] {
        agents::connected(port, admin, name, log, Duration::from_secs(60))?;
    }
    let id = agents::open_session(port, admin, "box-a", &cwd, Duration::from_secs(60))?;
    let e0 = first_epoch(port, admin, &id)?;
    let text = "the twin's goal: never met by a copy left behind";
    op(port, admin, "goals.add", &json!({"agent": id, "goals": [{"text": text}]}))?;
    let stack = op(port, admin, "goals.get", &json!({"agent": id}))?;
    let goal_id = stack.get("items").and_then(Value::as_array).and_then(|l| l.iter().find(|g| field(g, "text") == text)).map(|g| field(g, "id").to_owned()).ok_or_else(|| anyhow::anyhow!("no goal added: {stack}"))?;
    record_kept(dir.path(), port, admin, "box-a")?;
    let copy: i64 = {
        let said = std::fs::read_to_string(cwd.join(".fake-claude-messages"))?;
        said.lines().find_map(|l| serde_json::from_str::<Value>(l).ok()?.get("pid")?.as_i64()).ok_or_else(|| anyhow::anyhow!("the copy's harness said no pid"))?
    };
    // box-a cut off: the session goes on on box-b, from kiln's copy.
    until(Duration::from_secs(120), || Ok((!connected(port, admin, "box-a")?).then_some(())))?;
    let (st, body) = http(port, "POST", &format!("/api/harness/sessions/{id}/move"), admin, Some(&json!({"to": "box-b"})))?;
    anyhow::ensure!((200..300).contains(&st) && body.contains("from_store"), "{st} {body}");
    session_chain(dir.path(), port, admin, &id, 2)?;
    // The copy, cut off and still at epoch e0, acts: its goal met and a CI run, through its runner
    // as its MCP server asks, and a run on box-c, as its own child.
    let sock = kiln::reach::local::path("box-a", &dir.path().join("box-a-state"));
    let ask = |name: &str, input: Value| -> anyhow::Result<String> {
        let mut c = Client::connect(&sock)?;
        c.send(&Req::Ask { json: json!({"op": "call", "session": id, "name": name, "input": input}).to_string() })?;
        Ok(match c.event()? {
            Some(Ev::Answer(a)) => a,
            Some(Ev::Error(e)) => e,
            other => format!("{other:?}"),
        })
    };
    let met = ask("goals.done", json!({"id": goal_id, "evidence": "said by a copy left behind"}))?;
    let ci = ask("ci.runs.trigger", json!({"repo": "hyper-light/kiln", "workflow": "ci.yaml", "ref": "main"}))?;
    let ran = dir.path().join("twin-ran");
    std::fs::write(cwd.join(format!(".fake-claude-acts-{copy}")), format!("{} run --on box-c -- touch {}\n", env!("CARGO_BIN_EXE_kiln"), ran.display()))?;
    let acted = until(Duration::from_secs(60), || Ok(std::fs::read_to_string(cwd.join(".fake-claude-acted")).ok().filter(|t| !t.is_empty())))?;
    // box-a back: its runner is told to let go of the copy, and what the copy tried is recorded.
    until(Duration::from_secs(150), || Ok(connected(port, admin, "box-a")?.then_some(())))?;
    let refused = until(Duration::from_secs(90), || {
        let (_, body) = http(port, "GET", "/api/transfers/refused", admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let of: Vec<Value> = v.get("refused").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter(|r| field(r, "act").contains(&id) && field(r, "came_from").starts_with(&format!("runner {}", machine("box-a")))).collect();
        Ok((of.len() >= 3).then_some(of))
    })
    .map_err(|e| anyhow::anyhow!("{e}: goals.done said {met}; ci said {ci}; the run said {acted}: {}", said_of(dir.path(), port, admin, &id)))?;
    let server_log = std::fs::read_to_string(dir.path().join("server.log")).unwrap_or_default();
    assert!(server_log.contains("its runner is told to let go of it") && server_log.contains(&id), "the copy was told to let go at its runner's first word");
    // None of it done: the goal is still to meet, no CI run started, nothing ran on box-c.
    let goals = op(port, admin, "goals.get", &json!({"agent": id}))?;
    let still = goals.get("items").and_then(Value::as_array).is_some_and(|l| l.iter().any(|g| field(g, "id") == goal_id && field(g, "status") != "met"));
    assert!(still, "the copy's goals.done wasn't applied: {goals}");
    let runs = op(port, admin, "ci.runs.list", &json!({}))?;
    assert!(runs.get("runs").and_then(Value::as_array).is_none_or(Vec::is_empty), "no CI run started: {runs}");
    assert!(!ran.exists(), "nothing ran on box-c: {acted}");
    assert!(refused.iter().any(|r| field(r, "act").contains("goals.done")) && refused.iter().any(|r| field(r, "act").contains("ci.runs.trigger")) && refused.iter().any(|r| field(r, "act").contains("a run on box-c")), "{refused:?}");
    // Each recorded with who signed it, the epoch it's from and the transfer that superseded it.
    let from_e0 = |r: &Value| r.get("epoch").and_then(Value::as_u64) == Some(e0) && field(r, "signer").contains(&format!("/agent/{id}/epoch/{e0}/machine/{}/", machine("box-a"))) && !field(r, "superseded_by").is_empty() && !field(r, "digest").is_empty();
    assert!(refused.iter().all(from_e0), "{refused:?}");
    // The session at its next epoch goes on untouched, on box-b.
    let v = session_chain(dir.path(), port, admin, &id, 2)?;
    assert!(field(&v, "held_by").contains(&format!("/agent/{id}/epoch/{}/machine/{}/", e0 + 1, machine("box-b"))), "{v}");
    Ok(())
}


/// W27-R08 (the Transfers plan's step 6): work handed between agents and to people, signed by
/// both sides. A plan run with an agent per step, side by side: each agent signs its receipt of
/// kiln's hand-on as it takes its step up, at its first turn. Step 1, tried again, goes to a new
/// agent, kiln taking it for the first, which, asking still as the step's agent, is refused and
/// recorded. Step 2, taken over by the owner from a device with no key, gets kiln's receipt for
/// them; step 3, taken over by sam with a key, gets sam's own. Step 1's agent starts a subagent for
/// a task it hands on signed, which kiln passes on unchanged and the subagent signs for as it takes
/// it up. No transfer without both sides' signatures: every chain walks unbroken, none left open.
#[test]
fn w27_r08_work_handed() -> anyhow::Result<()> {
    use kiln::reach::local::{Client, Ev, Req};
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let cwd = dir.path().join("work-here");
    let _harnesses = Harnesses::new(&cwd, &dir.path().join("home"))?;
    let claude = dir.path().join("home").join(".claude").to_string_lossy().into_owned();
    let env = [("FAKE_CLAUDE_RECORD", "1"), ("CLAUDE_CONFIG_DIR", claude.as_str())];
    let (_a, log_a) = agents::runner_with(dir.path(), port, "box-a", None, TALK, &env)?;
    agents::connected(port, admin, "box-a", &log_a, Duration::from_secs(60))?;
    let json_of = |what: &str, (st, body): (u16, String)| -> anyhow::Result<Value> {
        anyhow::ensure!((200..300).contains(&st), "{what}: {st} {body}");
        Ok(serde_json::from_str(&body)?)
    };
    let plan = json_of("the plan", http(port, "POST", "/api/plans", admin, Some(&json!({ "title": "work handed", "body": "- build it\n- test it\n- ship it\n- document it\n- announce it\n" })))?)?;
    let at = cwd.to_string_lossy().into_owned();
    let ran = json_of("the run", http(port, "POST", &format!("/api/plans/{}/run", field(&plan, "id")), admin, Some(&json!({ "mode": "agents", "parallel": true, "new": { "on": "box-a", "kind": "claude", "cwd": at } })))?)?;
    let run = field(&ran, "run").to_owned();
    let chain_of = |name: &str| -> anyhow::Result<Value> {
        let (_, body) = http(port, "GET", &format!("/api/custody?name={}", name.replace(' ', "%20").replace('\'', "%27")), admin, None)?;
        Ok(serde_json::from_str(&body).unwrap_or_default())
    };
    let hops = |v: &Value| -> Vec<Value> { v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default() };
    let kinds = |v: &Value| -> Vec<String> { hops(v).iter().map(|h| field(h, "kind").to_owned()).collect() };
    let received = |name: &str, by: &str| -> anyhow::Result<Value> {
        until(Duration::from_secs(120), || {
            let v = chain_of(name)?;
            let first = hops(&v).first().cloned().unwrap_or_default();
            Ok((v.get("unbroken") == Some(&json!(true)) && field(&first, "kind") == "received" && field(&first, "custodian").contains(by)).then_some(v))
        })
        .map_err(|e| anyhow::anyhow!("{e}: {name}: {}\n{}", chain_of(name).unwrap_or_default(), said_of(dir.path(), port, admin, by)))
    };
    let agent_of = |n: u64| -> anyhow::Result<String> {
        let v = json_of("the run", http(port, "GET", &format!("/api/plans/runs/{run}"), admin, None)?)?;
        Ok(v.get("steps").and_then(Value::as_array).and_then(|s| s.iter().find(|x| x.get("n") == Some(&json!(n)))).map(|x| field(x, "agent_id").to_owned()).unwrap_or_default())
    };
    let step = |n: u64| format!("plan run {run} step {n}");
    // Each step's agent takes it up at its first turn: kiln's hand-on, and its receipt.
    let mut firsts = Vec::new();
    for n in 1..=5 {
        let a = until(Duration::from_secs(120), || Ok(Some(agent_of(n)?).filter(|a| !a.is_empty())))?;
        let v = received(&step(n), &format!("/agent/{a}/"))?;
        assert_eq!(kinds(&v), ["received", "handed-on"], "{v}");
        firsts.push(a);
    }
    let first = firsts.first().cloned().unwrap_or_default();
    // Step 1 tried again: kiln takes it for its agent and hands it to the next, which signs.
    json_of("the retry", http(port, "POST", &format!("/api/plans/runs/{run}/steps/1/retry"), admin, Some(&json!({})))?)?;
    let second = until(Duration::from_secs(120), || Ok(Some(agent_of(1)?).filter(|a| !a.is_empty() && *a != first)))?;
    let v = received(&step(1), &format!("/agent/{second}/"))?;
    assert_eq!(kinds(&v), ["received", "taken", "received", "handed-on"], "{v}");
    let take = hops(&v).get(1).map(|h| field(h, "record").to_owned()).unwrap_or_default();
    // The first agent, asking still as the step's: refused, and recorded with the take.
    let sock = kiln::reach::local::path("box-a", &dir.path().join("box-a-state"));
    let ask_as = |session: &str, name: &str, input: Value| -> anyhow::Result<String> {
        let mut c = Client::connect(&sock)?;
        c.send(&Req::Ask { json: json!({ "op": "call", "session": session, "name": name, "input": input }).to_string() })?;
        Ok(match c.event()? {
            Some(Ev::Answer(a)) => a,
            Some(Ev::Error(e)) => e,
            other => format!("{other:?}"),
        })
    };
    let said = ask_as(&first, "goals.done", json!({ "evidence": "said by the try before" }))?;
    assert!(said.contains("refused") && said.contains("no longer holds"), "{said}");
    let refused = json_of("the refusals", http(port, "GET", "/api/transfers/refused", admin, None)?)?;
    let of_first: Vec<Value> = refused.get("refused").and_then(Value::as_array).into_iter().flatten().filter(|r| field(r, "act").contains(&first)).cloned().collect();
    assert!(of_first.iter().any(|r| field(r, "superseded_by") == take && field(r, "came_from").starts_with(&format!("runner {}", machine("box-a")))), "{of_first:?}");
    // Step 2 taken over by the owner, from a device with no key: kiln's receipt for them.
    let took = json_of("step 2 taken over", http(port, "POST", &format!("/api/plans/runs/{run}/steps/2/take"), admin, None)?)?;
    assert!(took.pointer("/custody/kiln_signed_for_you").is_some_and(Value::is_string), "{took}");
    let v = chain_of(&step(2))?;
    let kiln_for = hops(&v).first().cloned().unwrap_or_default();
    assert_eq!(kinds(&v), ["received", "taken", "received", "handed-on"], "{v}");
    assert!(field(&kiln_for, "custodian").ends_with("/server") && field(&kiln_for, "taken_for").ends_with("/person/admin") && field(&kiln_for, "why").contains("no key on the device that asked"), "kiln for the owner, never the owner: {kiln_for}");
    // Step 3 taken over by sam, with a key: sam's own receipt.
    let sam = person(port, admin, "sam")?;
    let sams_key = certified(port, Auth::Cookie(&sam))?;
    let (st, body) = ask(port, "POST", &format!("/api/plans/runs/{run}/steps/3/take"), Auth::Cookie(&sam), Some(&json!({ "signs": true })))?;
    anyhow::ensure!(st == 200, "step 3 taken over by sam: {st} {body}");
    let c = serde_json::from_str::<Value>(&body)?.get("custody").cloned().unwrap_or_default();
    let subject: Subject = serde_json::from_value(c.get("subject").cloned().unwrap_or_default())?;
    let b = Body { from: Some(field(&c, "from").to_owned()), why: "step 3, taken over".into(), ..Body::new(Kind::Received, "") };
    let receipt = sams_key.sign(&Statement { subject: vec![subject], follows: vec![field(&c, "hand_on").to_owned()], ..Statement::new(to_value(&b)) })?;
    let (st, body) = ask(port, "POST", &format!("/api/plans/runs/{run}/steps/3/receipt"), Auth::Cookie(&sam), Some(&json!({ "receipt": receipt })))?;
    anyhow::ensure!(st == 200, "sam's receipt: {st} {body}");
    let v = chain_of(&step(3))?;
    let sams = hops(&v).first().cloned().unwrap_or_default();
    assert!(field(&sams, "custodian") == sams_key.uri && sams.get("taken_for").is_none_or(Value::is_null), "sam's own: {sams}");
    // Step 4 taken over by sam in a browser, which signs with its own key: kiln says what to sign
    // (as it does a verdict), the browser signs those bytes, and the receipt is sam's own.
    let (st, body) = ask(port, "POST", &format!("/api/plans/runs/{run}/steps/4/take"), Auth::Cookie(&sam), Some(&json!({ "signs": true })))?;
    anyhow::ensure!(st == 200, "step 4 taken over by sam: {st} {body}");
    let (st, body) = ask(port, "POST", &format!("/api/plans/runs/{run}/steps/4/statement"), Auth::Cookie(&sam), Some(&json!({})))?;
    anyhow::ensure!(st == 200, "what sam signs: {st} {body}");
    let asked: Value = serde_json::from_str(&body)?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let (payload, pae) = (base64::Engine::decode(&b64, field(&asked, "payload"))?, base64::Engine::decode(&b64, field(&asked, "pae"))?);
    let signed = kiln_ci::identity::statement::bundle(field(&asked, "payload_type"), &payload, &rcgen::SigningKey::sign(&sams_key.key, &pae)?, &sams_key.der);
    let (st, body) = ask(port, "POST", &format!("/api/plans/runs/{run}/steps/4/receipt"), Auth::Cookie(&sam), Some(&json!({ "receipt": signed })))?;
    anyhow::ensure!(st == 200, "sam's receipt, signed in the browser: {st} {body}");
    let v = chain_of(&step(4))?;
    let browsers = hops(&v).first().cloned().unwrap_or_default();
    assert!(field(&browsers, "custodian") == sams_key.uri && browsers.get("taken_for").is_none_or(Value::is_null), "sam's own, signed in the browser: {browsers}");
    // Step 5 taken over by sam in a browser that can't sign: kiln's receipt for him, saying why.
    let (st, body) = ask(port, "POST", &format!("/api/plans/runs/{run}/steps/5/take"), Auth::Cookie(&sam), Some(&json!({ "signs": true })))?;
    anyhow::ensure!(st == 200, "step 5 taken over by sam: {st} {body}");
    let missing = "this page isn't secure (https), so this browser has no key";
    let (st, body) = ask(port, "POST", &format!("/api/plans/runs/{run}/steps/5/receipt"), Auth::Cookie(&sam), Some(&json!({ "unsigned": missing })))?;
    anyhow::ensure!(st == 200, "kiln's receipt for sam: {st} {body}");
    let v = chain_of(&step(5))?;
    let kiln_for_sam = hops(&v).first().cloned().unwrap_or_default();
    assert!(field(&kiln_for_sam, "custodian").ends_with("/server") && field(&kiln_for_sam, "taken_for").ends_with("/person/sam") && field(&kiln_for_sam, "why").contains(missing), "kiln for sam, saying why: {kiln_for_sam}");
    // A subagent: step 1's agent starts one for a task, handing it on signed.
    let started = ask_as(&second, "agents.start", json!({ "kind": "claude", "prompt": "count the tests", "cwd": at }))?;
    let child = serde_json::from_str::<Value>(&started).ok().and_then(|v| v.pointer("/ok/agents/0/id").and_then(Value::as_str).map(str::to_owned)).ok_or_else(|| anyhow::anyhow!("no subagent started: {started}"))?;
    let task = format!("a subagent's task from agent {second}");
    let v = received(&task, &format!("/agent/{child}/"))?;
    assert_eq!(kinds(&v), ["received", "passed-on", "handed-on"], "{v}");
    let custodians: Vec<String> = hops(&v).iter().map(|h| field(h, "custodian").to_owned()).collect();
    assert!(custodians.get(1).is_some_and(|c| c.ends_with("/server")) && custodians.get(2).is_some_and(|c| c.contains(&format!("/agent/{second}/"))), "{custodians:?}");
    // No transfer without both sides' signatures: every chain unbroken, none open.
    for name in [step(1), step(2), step(3), step(4), step(5), task] {
        let v = chain_of(&name)?;
        assert!(v.get("unbroken") == Some(&json!(true)) && v.get("open") != Some(&json!(true)) && half_named(&v) == 0, "{name}: {v}");
    }
    Ok(())
}

/// `git <args>` in `dir`, as a test's own git (no system config, no prompts): what it said.
fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = std::process::Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper=", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    anyhow::ensure!(o.status.success(), "git {args:?}: {out}");
    Ok(out)
}

/// User `name`, a developer of `repo`, with a personal access token of their own: the token.
fn developer(port: u16, admin: &str, name: &str, repo: &str) -> anyhow::Result<String> {
    let (st, body) = http(port, "POST", "/api/users", admin, Some(&json!({ "name": name, "grants": [format!("developer@repo:{repo}")] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, body) = http(port, "POST", "/api/tokens", admin, Some(&json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}'s token: {st} {body}");
    serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))
}

/// A native repository `repo` with a first commit on main, and a clone of it: (git's home, the
/// clone).
fn native(dir: &Path, port: u16, admin: &str, repo: &str) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    let (st, body) = http(port, "POST", "/api/repos", admin, Some(&json!({ "name": repo, "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{repo}: {st} {body}");
    let (home, work) = (dir.join("githome"), dir.join("clone"));
    std::fs::create_dir_all(&home)?;
    git(&home, dir, &["clone", "-q", &format!("http://x:{admin}@127.0.0.1:{port}/git/{repo}.git"), "clone"])?;
    std::fs::write(work.join("README.md"), "notes\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", "origin", "HEAD:main"])?;
    Ok((home, work))
}

/// The custody chain of the thing named `name`, walked now.
fn chain_named(port: u16, admin: &str, name: &str) -> anyhow::Result<Value> {
    let (_, body) = http(port, "GET", &format!("/api/custody?name={}", name.replace(' ', "%20")), admin, None)?;
    Ok(serde_json::from_str(&body).unwrap_or_default())
}

/// W27-R08 (the Transfers plan's step 7): a change changes hands signed. kim gives D1 to sam, who
/// takes it; lee commandeers it: two transfers, each with both sides (kiln's take for the side not
/// there, its receipt for a person with no key on the device that asked). sam, its author no more,
/// pushing a version of it as its author, or abandoning it, is refused and recorded with the
/// transfer that superseded them.
#[test]
fn w27_r08_change_hands() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin, repo) = (s.port, s.admin.as_str(), "acme/notes");
    let (home, work) = native(dir.path(), port, admin, repo)?;
    let (kim, sam, lee) = (developer(port, admin, "kim", repo)?, developer(port, admin, "sam", repo)?, developer(port, admin, "lee", repo)?);
    let origin = |token: &str| format!("http://x:{token}@127.0.0.1:{port}/git/{repo}.git");
    git(&home, &work, &["checkout", "-qb", "change"])?;
    std::fs::write(work.join("notes.txt"), "a note\n")?;
    git(&home, &work, &["add", "notes.txt"])?;
    git(&home, &work, &["commit", "-qm", "A note\n\nChange-Id: I5555555555555555555555555555555555555555"])?;
    git(&home, &work, &["push", "-q", &origin(&kim), "HEAD:refs/for/main"])?;
    let author = || -> anyhow::Result<String> {
        let (_, body) = http(port, "GET", "/api/diffs/D1", admin, None)?;
        Ok(field(&serde_json::from_str::<Value>(&body)?, "author").to_owned())
    };
    anyhow::ensure!(author()? == "user:kim", "{}", author()?);
    // Given by kim, taken by sam.
    let (st, body) = http(port, "POST", "/api/diffs/D1/give", &kim, Some(&json!({ "to": "user:sam" })))?;
    anyhow::ensure!(st == 200, "kim gives D1: {st} {body}");
    anyhow::ensure!(author()? == "user:kim", "given, it's kim's until sam takes it");
    let (st, body) = http(port, "POST", "/api/diffs/D1/take", &sam, None)?;
    anyhow::ensure!(st == 200, "sam takes D1: {st} {body}");
    anyhow::ensure!(author()? == "user:sam", "{}", author()?);
    // Commandeered by lee.
    let (st, body) = http(port, "POST", "/api/diffs/D1/commandeer", &lee, None)?;
    anyhow::ensure!(st == 200, "lee commandeers D1: {st} {body}");
    anyhow::ensure!(author()? == "user:lee", "{}", author()?);
    let v = chain_named(port, admin, &format!("change {repo} D1"))?;
    let kinds: Vec<&str> = v.get("hops").and_then(Value::as_array).into_iter().flatten().filter_map(|h| h.get("kind").and_then(Value::as_str)).collect();
    assert!(v.get("unbroken") == Some(&json!(true)) && kinds == ["received", "taken", "received", "taken"] && half_named(&v) == 0, "two transfers, both sides each: {v}");
    let take_from_sam = v.pointer("/hops/1/record").and_then(Value::as_str).unwrap_or_default().to_owned();
    // sam, its author no more: a version pushed as its author, and abandoning it, refused.
    std::fs::write(work.join("notes.txt"), "a note, by its author before\n")?;
    git(&home, &work, &["commit", "-q", "--amend", "-am", "A note\n\nChange-Id: I5555555555555555555555555555555555555555"])?;
    let pushed = git(&home, &work, &["push", "-q", &origin(&sam), "HEAD:refs/for/main"]);
    assert!(pushed.as_ref().is_err_and(|e| e.to_string().contains("changed hands")), "{pushed:?}");
    let (st, body) = http(port, "POST", "/api/diffs/D1/abandon", &sam, None)?;
    assert!(st == 403 && body.contains("changed hands"), "{st} {body}");
    let (_, body) = http(port, "GET", "/api/transfers/refused", admin, None)?;
    let refused: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("refused").and_then(Value::as_array).cloned().unwrap_or_default();
    let of_sam: Vec<&Value> = refused.iter().filter(|r| field(r, "signer").ends_with("/person/sam") && field(r, "superseded_by") == take_from_sam).collect();
    assert!(of_sam.iter().any(|r| field(r, "act").starts_with("push of a version")) && of_sam.iter().any(|r| field(r, "act").starts_with("abandon")), "{refused:?}");
    anyhow::ensure!(author()? == "user:lee", "nothing sam did took");
    Ok(())
}

/// W27-R08 (the Transfers plan's step 7): a review request passed on changes hands signed. rev1
/// passes D1's to rev2, who takes it: the request moves, with both sides. rev2 passes it on to rev3,
/// who never takes it: it stays rev2's.
#[test]
fn w27_r08_review_passed_on() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin, repo) = (s.port, s.admin.as_str(), "acme/notes");
    let (home, work) = native(dir.path(), port, admin, repo)?;
    let kim = developer(port, admin, "kim", repo)?;
    let (rev1, rev2, _rev3) = (developer(port, admin, "rev1", repo)?, developer(port, admin, "rev2", repo)?, developer(port, admin, "rev3", repo)?);
    git(&home, &work, &["checkout", "-qb", "change"])?;
    std::fs::write(work.join("notes.txt"), "a note\n")?;
    git(&home, &work, &["add", "notes.txt"])?;
    git(&home, &work, &["commit", "-qm", "A note\n\nChange-Id: I6666666666666666666666666666666666666666"])?;
    git(&home, &work, &["push", "-q", "-o", "reviewer=user:rev1", &format!("http://x:{kim}@127.0.0.1:{port}/git/{repo}.git"), "HEAD:refs/for/main"])?;
    let reviewers = || -> anyhow::Result<Vec<String>> {
        let (_, body) = http(port, "GET", "/api/diffs/D1", admin, None)?;
        let v: Value = serde_json::from_str(&body)?;
        let mut r: Vec<String> = v.get("reviewers").and_then(Value::as_array).into_iter().flatten().filter_map(|r| r.get("reviewer").or_else(|| r.get("name")).and_then(Value::as_str).map(str::to_owned)).collect();
        r.sort();
        Ok(r)
    };
    anyhow::ensure!(reviewers()? == ["user:rev1"], "{:?}", reviewers()?);
    // Passed on and taken: the request moves.
    let (st, body) = http(port, "POST", "/api/diffs/D1/reviewers/pass", &rev1, Some(&json!({ "to": "user:rev2" })))?;
    anyhow::ensure!(st == 200, "rev1 passes D1's review: {st} {body}");
    anyhow::ensure!(reviewers()? == ["user:rev1"], "passed, it's rev1's until rev2 takes it");
    let (st, body) = http(port, "POST", "/api/diffs/D1/reviewers/take", &rev2, None)?;
    anyhow::ensure!(st == 200, "rev2 takes it: {st} {body}");
    anyhow::ensure!(reviewers()? == ["user:rev2"], "{:?}", reviewers()?);
    let name = format!("review request of {repo} D1");
    let v = chain_named(port, admin, &name)?;
    let kinds: Vec<&str> = v.get("hops").and_then(Value::as_array).into_iter().flatten().filter_map(|h| h.get("kind").and_then(Value::as_str)).collect();
    assert!(v.get("unbroken") == Some(&json!(true)) && kinds == ["received", "taken"] && half_named(&v) == 0, "moved with both sides: {v}");
    // Passed on again, never taken: it stays rev2's.
    let (st, body) = http(port, "POST", "/api/diffs/D1/reviewers/pass", &rev2, Some(&json!({ "to": "user:rev3" })))?;
    anyhow::ensure!(st == 200, "rev2 passes it on: {st} {body}");
    anyhow::ensure!(reviewers()? == ["user:rev2"], "never taken, it stays rev2's: {:?}", reviewers()?);
    let v = chain_named(port, admin, &name)?;
    assert!(v.get("open") == Some(&json!(true)), "the pass to rev3 waits, answered by no one: {v}");
    Ok(())
}

/// W27-R08, with W27-R04 (the Transfers plan's step 7): an approval carried across a rebase-only
/// revision is a transfer that says so. Kim approves D1's v2, signed with her own key. Two rewords
/// and a rebase onto a moved trunk keep its verbatim patch-id, so at v5 her approval is carried:
/// kiln signs the carry from v2 to v5 (its hand-on and its receipt, each naming its version's
/// commit and base) and says "carried from v2: rebase only, patch-id …, base … to …", her verdict
/// still hers and verifying. v6 changes the code and v7 only its whitespace: carried to neither,
/// each saying why.
#[test]
fn w27_ac04_approval_carried() -> anyhow::Result<()> {
    use base64::Engine as _;
    use rcgen::SigningKey as _;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin, repo) = (s.port, s.admin.as_str(), "acme/review");
    let (home, work) = native(dir.path(), port, admin, repo)?;
    let kim = developer(port, admin, "kim", repo)?;
    let kims_key = certified(port, Auth::Token(&kim))?;
    let origin = format!("http://x:{admin}@127.0.0.1:{port}/git/{repo}.git");
    let message = "f returns two\n\nChange-Id: I4242424242424242424242424242424242424242\n";
    let push = |n: u32| -> anyhow::Result<()> {
        let said = git(&home, &work, &["push", &origin, "HEAD:refs/for/main"])?;
        anyhow::ensure!(said.contains(&format!("D1 v{n}")), "v{n}: {said}");
        Ok(())
    };
    std::fs::write(work.join("a.py"), "def f():\n    return 1\n")?;
    std::fs::write(work.join("z.txt"), "far away\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "f, and a file far away"])?;
    git(&home, &work, &["push", "-q", &origin, "HEAD:main"])?;
    git(&home, &work, &["checkout", "-qb", "d1"])?;
    std::fs::write(work.join("a.py"), "def f():\n    return 2\n")?;
    git(&home, &work, &["commit", "-qam", message])?;
    push(1)?;
    std::fs::write(work.join("a.py"), "def f():\n    return 2  # two\n")?;
    git(&home, &work, &["commit", "-q", "--amend", "-a", "-m", message])?;
    push(2)?;
    // Kim approves v2, signed with her own key.
    let (st, body) = http(port, "POST", "/api/diffs/D1/statement", &kim, Some(&json!({ "kind": "verdict", "action": "accept", "body": "looks right" })))?;
    anyhow::ensure!(st == 200, "what kim signs: {st} {body}");
    let asked: Value = serde_json::from_str(&body)?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let (payload, pae) = (b64.decode(field(&asked, "payload"))?, b64.decode(field(&asked, "pae"))?);
    let signed = kiln_ci::identity::statement::bundle(field(&asked, "payload_type"), &payload, &kims_key.key.sign(&pae)?, &kims_key.der);
    let (st, body) = http(port, "POST", "/api/diffs/D1/review", &kim, Some(&json!({ "action": "accept", "body": "looks right", "signed": signed })))?;
    anyhow::ensure!(st == 200, "kim's signed approval: {st} {body}");
    let kims = || -> anyhow::Result<Value> {
        let (st, body) = http(port, "GET", "/api/diffs/D1/verdicts", admin, None)?;
        anyhow::ensure!(st == 200, "verdicts: {st} {body}");
        let verdicts = serde_json::from_str::<Value>(&body)?.get("verdicts").and_then(Value::as_array).cloned().unwrap_or_default();
        verdicts.into_iter().rev().find(|v| field(v, "reviewer") == "user:kim").ok_or_else(|| anyhow::anyhow!("no verdict of kim's: {body}"))
    };
    assert_eq!(field(&kims()?, "standing"), "given");
    // Two rewords, then a rebase onto a moved trunk: the same verbatim patch-id.
    for (n, words) in [(3, "f returns 2"), (4, "f returns two, as asked")] {
        git(&home, &work, &["commit", "-q", "--amend", "-m", &format!("{words}\n\nChange-Id: I4242424242424242424242424242424242424242\n")])?;
        push(n)?;
    }
    git(&home, &work, &["checkout", "-q", "main"])?;
    std::fs::write(work.join("z.txt"), "far away, moved\n")?;
    git(&home, &work, &["commit", "-qam", "the trunk moves"])?;
    git(&home, &work, &["push", "-q", &origin, "HEAD:main"])?;
    git(&home, &work, &["checkout", "-q", "d1"])?;
    git(&home, &work, &["rebase", "-q", "main"])?;
    push(5)?;
    let v = kims()?;
    assert_eq!((field(&v, "standing"), v.get("on")), ("carried", Some(&json!(5))), "{v}");
    let carried = v.get("carried").cloned().unwrap_or_default();
    let (from_base, to_base) = (field(&carried, "base_from"), field(&carried, "base_to"));
    assert!(carried.get("from") == Some(&json!(2)) && !from_base.is_empty() && from_base != to_base, "carried from v2 across the rebase: {v}");
    let short = |s: &str| s.get(..7).unwrap_or(s).to_owned();
    let said = field(&carried, "said");
    assert_eq!(said, format!("carried from v2: rebase only, patch-id {}…, base {} to {}", short(field(&carried, "patch_id")), short(from_base), short(to_base)), "{v}");
    // The carry is a transfer: kiln's hand-on for v2 and its receipt for v5, walking unbroken.
    let (st, body) = http(port, "GET", &format!("/api/custody?digest={}", field(&carried, "record")), admin, None)?;
    anyhow::ensure!(st == 200, "the carry's chain: {st} {body}");
    let chain: Value = serde_json::from_str(&body)?;
    assert!(chain.get("unbroken") == Some(&json!(true)) && half_named(&chain) == 0, "{chain}");
    let hops = chain.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
    let sides: Vec<(&str, &str, &str)> = hops.iter().map(|h| (field(h, "kind"), field(h, "component"), field(h, "why"))).collect();
    assert!(matches!(sides.as_slice(), [(r, to, w1), (h, from, w2)] if *r == "received" && *h == "handed-on" && to.starts_with("v5 at ") && from.starts_with("v2 at ") && *w1 == said && *w2 == said), "{sides:?}");
    // Her verdict is still hers, as kiln checked it again as it carried it.
    assert_eq!(field(&v, "signer"), kims_key.uri, "{v}");
    // v6 changes the code: not carried, saying why.
    std::fs::write(work.join("a.py"), "def f():\n    return 3\n")?;
    git(&home, &work, &["commit", "-q", "--amend", "-a", "-m", message])?;
    push(6)?;
    let v = kims()?;
    assert!(field(&v, "standing") == "not" && field(&v, "why").contains("v6 changed the code") && v.get("carried").is_none(), "{v}");
    // v7 puts back v5's code but for a space at a line's end: whitespace is code here too.
    std::fs::write(work.join("a.py"), "def f():\n    return 2  # two \n")?;
    git(&home, &work, &["commit", "-q", "--amend", "-a", "-m", message])?;
    push(7)?;
    let v = kims()?;
    assert!(field(&v, "standing") == "not" && field(&v, "why").contains("v7 changed the code") && v.get("carried").is_none(), "{v}");
    Ok(())
}

/// W27-R08 (the Transfers plan's step 12): custody between machines, a part for each path of
/// docs/ci.md W27's inventory, each a helper of its own the test calls in turn on one kiln and its
/// runners: a runner's objects from its peers ([`peers_objects`]), kiln's store of bytes
/// ([`store_of_bytes`]), the conversation feed ([`feed_batches`]), and kiln run's sync and a reach
/// run ([`synced_runs`]). Each signed by both sides and walking unbroken, and each changed on its
/// way (a test build's faults: box-b's `peer_object_changed` and `feed_changed`, the server's
/// `blob_changed`, box-d's `sync_file_changed`) refused where it would be used and recorded as a
/// break at its hop. Failing, it keeps its folder (the server's and each box's logs and state) and
/// says where.
#[cfg(feature = "chaos")]
#[test]
fn w27_r08_custody_between_machines() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    match between_machines(dir.path()) {
        Ok(()) => Ok(()),
        Err(e) => Err(e.context(format!("its folder, the server's and each box's logs among it, is kept at {}", dir.keep().display()))),
    }
}

/// [`w27_r08_custody_between_machines`] in `dir`: every box started together, each path's part once
/// every box's reach directory holds the others.
#[cfg(feature = "chaos")]
fn between_machines(dir: &Path) -> anyhow::Result<()> {
    let changed = format!("blob_changed=binaries/kiln-{CHANGED_BINARY}");
    let s = common::start_env(dir, &Options::default(), false, &[("KILN_CHAOS", changed.as_str())])?;
    let (port, admin) = (s.port, s.admin.as_str());
    let cwd = dir.join("work-here");
    let _harnesses = Harnesses::new(&cwd, &dir.join("home"))?;
    let claude = dir.join("home").join(".claude").to_string_lossy().into_owned();
    let env = [("FAKE_CLAUDE_RECORD", "1"), ("CLAUDE_CONFIG_DIR", claude.as_str())];
    // Every box started before any is waited for. kiln run's (synced_runs): box-c hosts a session,
    // box-d's syncs change on their way.
    let (_a, log_a) = agents::runner_with(dir, port, "box-a", None, TALK, &env)?;
    let (_b, log_b) = agents::runner_with(dir, port, "box-b", Some("peer_object_changed,feed_changed"), TALK, &env)?;
    let (_c, log_c) = agents::runner_with(dir, port, "box-c", None, TALK, &env)?;
    let (_d, log_d) = agents::runner_with(dir, port, "box-d", Some("sync_file_changed"), TALK, &[])?;
    let boxes = [("box-a", &log_a), ("box-b", &log_b), ("box-c", &log_c), ("box-d", &log_d)];
    for (name, log) in boxes {
        agents::connected(port, admin, name, log, Duration::from_secs(60))?;
    }
    reach_directories_hold(dir, &boxes.map(|(name, _)| name), Duration::from_secs(90))?;
    peers_objects(dir, port, admin)?;
    store_of_bytes(dir, port, admin)?;
    feed_batches(dir, port, admin, &cwd)?;
    synced_runs(dir, port, admin)?;
    Ok(())
}

/// Until each of `boxes` sees every other in its reach directory, taking reach links: the directory
/// a runner checks a link against, which it keeps 30 s (one that announced after its last read is
/// refused till then, kiln's bug to fix). Asked over each box's own reach socket, before the first
/// link.
#[cfg(feature = "chaos")]
fn reach_directories_hold(dir: &Path, boxes: &[&str], within: Duration) -> anyhow::Result<()> {
    use kiln::reach::local::{Client, Ev, Req};
    for name in boxes {
        let mut last = String::new();
        until(within, || {
            // A socket not up yet, or no directory in its answer: ask again.
            let Ok(mut c) = Client::connect(&kiln::reach::local::path(name, &dir.join(format!("{name}-state")))) else { return Ok(None) };
            if c.send(&Req::Runners { want: None }).is_err() {
                return Ok(None);
            }
            let json = loop {
                match c.event() {
                    Ok(Some(Ev::Runners(json))) => break json,
                    Ok(Some(Ev::Error(_)) | None) | Err(_) => return Ok(None),
                    Ok(Some(_)) => {}
                }
            };
            let v: Value = serde_json::from_str(&json)?;
            let seen: Vec<&str> = v.get("runners").and_then(Value::as_array).into_iter().flatten().filter_map(|r| r.pointer("/runner/name").and_then(Value::as_str)).collect();
            let all = boxes.iter().all(|b| b == name || seen.contains(b));
            last = json;
            Ok(all.then_some(()))
        })
        .map_err(|e| anyhow::anyhow!("{name}'s reach directory doesn't hold the others: {e}; it says {last}"))?;
    }
    Ok(())
}

/// The platform whose binary the server's `blob_changed` changes as it's read
/// ([`w27_r08_custody_between_machines`]).
#[cfg(feature = "chaos")]
const CHANGED_BINARY: &str = "aarch64-pc-windows-msvc";

/// How a record names runner `name`'s process, `/machine/<its machine id>/`: kiln knows a runner
/// by its machine id (docs/machines.md), a test's made from its name (`common::machine`).
fn on(name: &str) -> String {
    format!("/machine/{}/", machine(name))
}

/// The custody of record `record` (its digest) as kiln took it: its signer.
#[cfg(feature = "chaos")]
fn custodian_of(dir: &Path, record: &str) -> anyhow::Result<String> {
    let db = rusqlite::Connection::open(dir.join("data/kiln.db"))?;
    Ok(db.query_row("SELECT custodian FROM custody WHERE digest = ?1", [record], |r| r.get(0))?)
}

/// The breaks kiln recorded, newest first.
#[cfg(feature = "chaos")]
fn breaks_now(port: u16, admin: &str) -> anyhow::Result<Vec<Value>> {
    let (_, body) = http(port, "GET", "/api/custody/breaks", admin, None)?;
    Ok(serde_json::from_str::<Value>(&body)?.get("breaks").and_then(Value::as_array).cloned().unwrap_or_default())
}

/// A thing's chain by its digest (`sha256:<hex>`), as kiln walks it now.
#[cfg(feature = "chaos")]
fn chain_of_digest(port: u16, admin: &str, digest: &str) -> anyhow::Result<Value> {
    let (_, body) = http(port, "GET", &format!("/api/custody?digest={digest}"), admin, None)?;
    Ok(serde_json::from_str(&body).unwrap_or_default())
}

/// A chain's hops: each one's kind, and its custodian (kiln's for someone: whom for).
#[cfg(feature = "chaos")]
fn hops_of(v: &Value) -> Vec<(String, String)> {
    v.get("hops").and_then(Value::as_array).into_iter().flatten().map(|h| (field(h, "kind").to_owned(), h.get("taken_for").and_then(Value::as_str).unwrap_or(field(h, "custodian")).to_owned())).collect()
}

/// A runner's objects from its peers (W24-R27): box-a and box-b each hold an object of acme/app's
/// cache. box-b's cache, asked for box-a's, fetches it from box-a over their link: box-a signs its
/// hand-on to box-b's runner process, and box-b its receipt of what the copy hashed to, so the
/// chain walks unbroken with both sides signed. What box-b sends changes on its way (its
/// `peer_object_changed`): box-a refuses the copy and keeps nothing of it, and kiln records the
/// break at that hop, between box-b's hand-on and box-a's receipt. Each cache is asked with a
/// token minted by its runner's key, as a build there holds one (W24-R24).
#[cfg(feature = "chaos")]
fn peers_objects(dir: &Path, port: u16, admin: &str) -> anyhow::Result<()> {
    use kiln::reach::local::{Client, Ev, Req, RunReq};
    use kiln::runner::work::client::{Conn, Endpoint};
    use kiln::runner::work::token::{Claims, Keyring};
    let repo = "acme/app";
    let open = |name: &str| -> anyhow::Result<(Conn, String)> {
        let mut c = Client::connect(&kiln::reach::local::path(name, &dir.join(format!("{name}-state"))))?;
        c.send(&Req::RunReuse(Box::new(RunReq { target: name.into(), command: "true".into(), ..RunReq::default() })))?;
        while !matches!(c.event()?, Some(Ev::Exit { .. } | Ev::Error(_)) | None) {}
        let at = dir.join(format!("{name}-work")).join("_cas");
        anyhow::ensure!(at.join("key").exists(), "{name}'s cache didn't start");
        let exp = i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs())? + 3600;
        let token = Keyring::open(&at)?.mint(&Claims { repo: repo.into(), domain: "trusted".into(), read: true, write: true, exp, grant: None });
        Ok((Conn::new(Endpoint::parse(&format!("unix:{}", at.join("cas.sock").display()))?), format!("Bearer {token}")))
    };
    let ((mut a, a_token), (mut b, b_token)) = (open("box-a")?, open("box-b")?);
    let put = |c: &mut Conn, token: &str, bytes: &[u8]| -> anyhow::Result<String> {
        let d = kiln::util::sha256_hex(bytes);
        let r = c.request("PUT", &format!("/_kiln/cas/v1/objects/{d}"), &[("authorization", token)], bytes)?;
        anyhow::ensure!(r.status == 201, "{d}: {} {}", r.status, String::from_utf8_lossy(&r.body));
        Ok(d)
    };
    let (x, y) = (put(&mut a, &a_token, b"an rlib from box-a's build")?, put(&mut b, &b_token, b"an rlib from box-b's build")?);
    // box-b asks its cache for box-a's object: it comes from box-a, once kiln's directory says
    // box-a holds it.
    let got = until(Duration::from_secs(60), || {
        let r = b.request("GET", &format!("/_kiln/cas/v1/objects/{x}"), &[("authorization", &b_token)], b"")?;
        Ok((r.status == 200).then_some(r.body))
    })?;
    assert_eq!(got, b"an rlib from box-a's build");
    // Both sides signed: box-a's hand-on to box-b's process, box-b's receipt following it.
    let chain_of = |d: &str| -> anyhow::Result<Value> {
        let (_, body) = http(port, "GET", &format!("/api/custody?digest=sha256:{d}"), admin, None)?;
        Ok(serde_json::from_str(&body).unwrap_or_default())
    };
    let hops = |v: &Value| -> Vec<(String, String)> { v.get("hops").and_then(Value::as_array).into_iter().flatten().map(|h| (field(h, "kind").to_owned(), field(h, "custodian").to_owned())).collect() };
    let v = until(Duration::from_secs(90), || {
        let v = chain_of(&x)?;
        Ok((v.get("unbroken") == Some(&json!(true)) && hops(&v).len() == 2).then_some(v))
    })
    .map_err(|e| anyhow::anyhow!("{e}: {}\n{}", chain_of(&x).unwrap_or_default(), said_of(dir, port, admin, &x)))?;
    let (b_uri, a_uri) = match hops(&v).as_slice() {
        [(r, taker), (h, giver)] if r == "received" && h == "handed-on" && taker.contains(&on("box-b")) && giver.contains(&on("box-a")) => (taker.clone(), giver.clone()),
        other => anyhow::bail!("box-b's receipt of box-a's hand-on: {other:?}"),
    };
    assert!(half_named(&v) == 0, "each hop names both sides, signed: {v}");
    // box-a asks its cache for box-b's object, which changes on its way: never served, nothing of
    // it kept, and the break recorded where box-a received other bytes than box-b handed on.
    let mut changed = b"an rlib from box-b's build".to_vec();
    if let Some(first) = changed.first_mut() {
        *first ^= 0x01;
    }
    let changed = kiln::util::sha256_hex(&changed);
    let broke = until(Duration::from_secs(90), || {
        let r = a.request("GET", &format!("/_kiln/cas/v1/objects/{y}"), &[("authorization", &a_token)], b"")?;
        anyhow::ensure!(r.status == 404, "a changed copy is never served: {} {}", r.status, String::from_utf8_lossy(&r.body));
        let (_, body) = http(port, "GET", "/api/custody/breaks", admin, None)?;
        let breaks: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("breaks").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(breaks.into_iter().find(|b| field(b, "kind") == "changed" && field(b, "detail").contains("object of acme/app")))
    })
    .map_err(|e| anyhow::anyhow!("{e}: {}", said_of(dir, port, admin, &y)))?;
    let db = rusqlite::Connection::open(dir.join("data/kiln.db"))?;
    let custodian = |d: &str| -> anyhow::Result<String> { Ok(db.query_row("SELECT custodian FROM custody WHERE digest = ?1", [d], |r| r.get(0))?) };
    let (record, follows) = (field(&broke, "record"), broke.pointer("/around/1").and_then(Value::as_str).unwrap_or_default());
    assert_eq!((custodian(record)?, custodian(follows)?), (a_uri, b_uri), "box-a's receipt of box-b's hand-on: {broke}");
    let r = a.request("HEAD", &format!("/_kiln/cas/v1/objects/{changed}"), &[("authorization", &a_token)], b"")?;
    assert_eq!(r.status, 404, "box-a kept nothing of the changed copy");
    Ok(())
}

/// W27-R08 (the Transfers plan's step 12, path f; docs/p2p.md P2): the cluster group's custody.
/// box-a's learner takes a copy of kiln.db, then follows the log. kiln hands the copy on to box-a's
/// runner process by its SHA-256, and box-a receives it so. Every 10 s while entries flow (a setting
/// changed every 2 s here), kiln hands box-a a checkpoint of the log, by the SHA-256 of the leaves of
/// the entries since box-a's period began; box-a signs its receipt of the one closing each period,
/// a minute or more apart, the periods running on one from the other, each walking from box-a's
/// receipt to kiln's hand-on. The server's `copy_changed`: its first copy came changed, refused, the
/// break recorded and walked at box-a's receipt, and another fetched. Its `entry_changed`: the
/// first entry it sent box-a came changed, box-a's next checkpoint didn't match, the break recorded
/// and walked at box-a's receipt, and box-a copied the database again. Failing, it keeps its folder
/// (the server's and box-a's logs among it) and says where.
#[cfg(feature = "chaos")]
#[test]
fn w27_r08_custody_between_machines_the_cluster_group() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    match cluster_group(dir.path()) {
        Ok(()) => Ok(()),
        Err(e) => Err(e.context(format!("its folder, the server's and box-a's logs among it, is kept at {}", dir.keep().display()))),
    }
}

/// [`w27_r08_custody_between_machines_the_cluster_group`] in `dir`.
#[cfg(feature = "chaos")]
fn cluster_group(dir: &Path) -> anyhow::Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let s = common::start_env(dir, &Options::default(), false, &[("KILN_CHAOS", "copy_changed,entry_changed")])?;
    let (port, admin) = (s.port, s.admin.clone());
    let (_a, log_a) = agents::runner_with(dir, port, "box-a", None, TALK, &[])?;
    agents::connected(port, &admin, "box-a", &log_a, Duration::from_secs(60))?;
    // Entries flow: one of kiln's settings changed every 2 s, till the test ends.
    let writing = Arc::new(AtomicBool::new(true));
    let writer = {
        let (writing, admin) = (writing.clone(), admin.clone());
        std::thread::spawn(move || {
            let mut n = 0u32;
            while writing.load(Ordering::SeqCst) {
                n = n.wrapping_add(1);
                let _ = http(port, "PUT", "/api/updates/skew", &admin, Some(&json!({ "full": 20 + n % 2, "upgrade": 40 })));
                std::thread::sleep(Duration::from_secs(2));
            }
        })
    };
    let checked = cluster_group_custody(dir, port, &admin);
    writing.store(false, Ordering::SeqCst);
    writer.join().ok();
    checked.map_err(|e| anyhow::anyhow!("{e:#}\n{}", cluster_said(dir)))
}

/// box-a's receipts of what kiln's cluster group handed it, oldest first: each record's digest, the
/// thing's name, and the hand-on it follows with when kiln took it.
#[cfg(feature = "chaos")]
fn cluster_receipts(dir: &Path, like: &str) -> anyhow::Result<Vec<(String, String, String, i64)>> {
    let db = rusqlite::Connection::open(dir.join("data/kiln.db"))?;
    let mut q = db.prepare(
        "SELECT c.digest, t.name, f.follows, h.taken_at FROM custody c JOIN custody_things t ON t.record = c.digest JOIN custody_follows f ON f.record = c.digest JOIN custody h ON h.digest = f.follows
         WHERE c.kind = 'received' AND c.custodian LIKE ?1 AND t.name LIKE ?2 ORDER BY c.taken_at, c.rowid",
    )?;
    let rows = q.query_map([format!("%{}%", on("box-a")).as_str(), like], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The first and last entries a stretch of the cluster log's name says ("entries 12 to 40 of kiln's
/// cluster log").
#[cfg(feature = "chaos")]
fn entries_named(name: &str) -> Option<(u64, u64)> {
    let rest = name.strip_prefix("entries ")?;
    let (first, rest) = rest.split_once(" to ")?;
    let (last, _) = rest.split_once(' ')?;
    Some((first.parse().ok()?, last.parse().ok()?))
}

/// What [`w27_r08_custody_between_machines_the_cluster_group`] checks, once box-a is connected.
#[cfg(feature = "chaos")]
fn cluster_group_custody(dir: &Path, port: u16, admin: &str) -> anyhow::Result<()> {
    // Two hops, both signed: box-a's receipt, and the hand-on of kiln's server (of box-a's
    // organization: `kiln://<org>/server`).
    let walks_from_box_a = |record: &str| -> anyhow::Result<()> {
        let v = chain_of_digest(port, admin, record)?;
        anyhow::ensure!(v.get("unbroken") == Some(&json!(true)), "{record}'s chain doesn't walk: {v}");
        let kilns = |taker: &str, giver: &str| taker.split('/').nth(2).is_some_and(|org| giver == format!("kiln://{org}/server"));
        match hops_of(&v).as_slice() {
            [(r, taker), (h, giver)] if r == "received" && h == "handed-on" && taker.contains(&on("box-a")) && kilns(taker, giver) => Ok(()),
            other => anyhow::bail!("box-a's receipt of kiln's hand-on: {other:?}"),
        }
    };
    let broken_at = |record: &str| -> anyhow::Result<()> {
        let v = chain_of_digest(port, admin, record)?;
        anyhow::ensure!(v.get("unbroken") == Some(&json!(false)) && v.pointer("/break/kind") == Some(&json!("changed")) && v.pointer("/break/record") == Some(&json!(record)), "the walk from {record} breaks there, changed: {v}");
        Ok(())
    };
    // The copy that came changed: refused, the break recorded at box-a's receipt of what came, which
    // kiln's walk finds again; then a copy taken as kiln handed it on.
    let copy_break = until(Duration::from_secs(120), || Ok(breaks_now(port, admin)?.into_iter().find(|b| field(b, "kind") == "changed" && field(b, "detail").contains("copy of kiln.db"))))?;
    let record = field(&copy_break, "record").to_owned();
    anyhow::ensure!(custodian_of(dir, &record)?.contains(&on("box-a")), "the break is at box-a's receipt: {copy_break}");
    broken_at(&record)?;
    let taken = until(Duration::from_secs(60), || Ok(cluster_receipts(dir, "copy of kiln.db at entry %")?.into_iter().find(|r| r.0 != record).map(|r| r.0)))?;
    walks_from_box_a(&taken)?;
    // The entry that came changed: box-a's next checkpoint didn't match, the break recorded at its
    // receipt of what it applied; and box-a copied the database again, taking that copy as handed on.
    let log_break = until(Duration::from_secs(120), || Ok(breaks_now(port, admin)?.into_iter().find(|b| field(b, "kind") == "changed" && field(b, "detail").contains("of kiln's cluster log"))))?;
    let broke = field(&log_break, "record").to_owned();
    anyhow::ensure!(custodian_of(dir, &broke)?.contains(&on("box-a")), "the break is at box-a's receipt: {log_break}");
    broken_at(&broke)?;
    until(Duration::from_secs(60), || {
        let copies = cluster_receipts(dir, "copy of kiln.db at entry %")?;
        Ok((copies.len() >= 3 && copies.last().is_some_and(|c| c.0 != record)).then_some(()))
    })
    .map_err(|e| anyhow::anyhow!("box-a's third copy, over the entry that came changed: {e}"))?;
    let last_copy = cluster_receipts(dir, "copy of kiln.db at entry %")?.last().map(|c| c.0.clone()).unwrap_or_default();
    walks_from_box_a(&last_copy)?;
    // The log's receipts after it: two, of periods closing a minute or more apart, the second
    // running on from the first, each walking from box-a's receipt to kiln's hand-on.
    let after = cluster_receipts(dir, "entries % of kiln's cluster log")?.iter().position(|r| r.0 == broke).unwrap_or_default();
    let receipts = until(Duration::from_secs(300), || {
        let all = cluster_receipts(dir, "entries % of kiln's cluster log")?;
        let since: Vec<_> = all.into_iter().skip(after + 1).collect();
        Ok((since.len() >= 2).then_some(since))
    })
    .map_err(|e| anyhow::anyhow!("two receipts of the log after box-a's copy: {e}"))?;
    let [first, second, ..] = receipts.as_slice() else { anyhow::bail!("two receipts: {receipts:?}") };
    for (record, ..) in [first, second] {
        walks_from_box_a(record)?;
    }
    let (a, b) = (entries_named(&first.1).unwrap_or_default(), entries_named(&second.1).unwrap_or_default());
    anyhow::ensure!(b.0 == a.1 + 1 && b.1 > b.0, "the second period runs on from the first: {} then {}", first.1, second.1);
    anyhow::ensure!(second.3 - first.3 >= 59, "kiln closed the periods a minute or more apart: its hand-ons at {} and {}", first.3, second.3);
    Ok(())
}

/// What the server and box-a logged of the cluster group and its custody: why the test of it
/// didn't go through.
#[cfg(feature = "chaos")]
fn cluster_said(dir: &Path) -> String {
    let mut names: Vec<std::path::PathBuf> = std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "log")).collect()).unwrap_or_default();
    names.sort();
    names
        .iter()
        .map(|n| {
            let text = std::fs::read_to_string(n).unwrap_or_default();
            let lines: Vec<&str> = text.lines().filter(|l| ["cluster", "replica", "copy of kiln.db", "custody", "chaos", "panicked", "ERROR"].iter().any(|w| l.contains(w))).collect();
            format!("{}:\n{}", n.display(), lines.get(lines.len().saturating_sub(80)..).unwrap_or(&[]).join("\n"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What kiln has of the relays' streams (`GET /api/custody/relays`), its records no receipt
/// answered `lapse` seconds on each recorded as a break naming its relay.
fn relays(port: u16, admin: &str, lapse: i64) -> anyhow::Result<Value> {
    let (st, body) = http(port, "GET", &format!("/api/custody/relays?lapse={lapse}"), admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    Ok(serde_json::from_str(&body)?)
}

/// Relay `relay`'s streams (by a part of its identity's URI) whose names start `stream`: how many
/// records kiln has on them, and how many numbers are missing between them.
fn stream_counts(v: &Value, relay: &str, stream: &str) -> (u64, u64) {
    let of: Vec<&Value> = v.get("streams").and_then(Value::as_array).into_iter().flatten().filter(|s| field(s, "relay").contains(relay) && field(s, "stream").starts_with(stream)).collect();
    let n = |s: &&Value, k: &str| s.get(k).and_then(Value::as_u64).unwrap_or_default();
    (of.iter().map(|s| n(s, "records")).sum(), of.iter().map(|s| n(s, "missing")).sum())
}

/// The breaks kiln recorded naming a relay (by a part of its identity's URI).
fn relay_breaks(port: u16, admin: &str, relay: &str) -> anyhow::Result<Vec<Value>> {
    let (st, body) = http(port, "GET", "/api/custody/breaks", admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    Ok(serde_json::from_str::<Value>(&body)?.get("breaks").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter(|b| field(b, "relay").contains(relay)).collect())
}

/// Kiln's receipts of what a relay passed on to `taker` (by a part of its URI), newest first.
fn receipts_by(dir: &Path, taker: &str) -> anyhow::Result<Vec<String>> {
    let db = rusqlite::Connection::open(dir.join("data/kiln.db"))?;
    let mut q = db.prepare("SELECT r.digest FROM custody r JOIN custody_follows f ON f.record = r.digest JOIN custody p ON p.digest = f.follows WHERE r.kind = 'received' AND p.kind = 'passed-on' AND r.custodian LIKE ?1 ORDER BY r.taken_at DESC")?;
    let rows = q.query_map([format!("%{taker}%")], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
    Ok(rows)
}

/// The kinds and custodians of the hops of the chain walked from record `digest`, once it walks
/// unbroken.
fn walked_hops(port: u16, admin: &str, digest: &str) -> anyhow::Result<Vec<(String, String)>> {
    let (st, body) = http(port, "GET", &format!("/api/custody?digest={digest}"), admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    anyhow::ensure!(v.get("unbroken") == Some(&json!(true)), "{digest}'s chain doesn't walk: {v}");
    Ok(v.get("hops").and_then(Value::as_array).into_iter().flatten().map(|h| (field(h, "kind").to_owned(), field(h, "custodian").to_owned())).collect())
}

/// Until runner `from`'s reach (in `dir`) has runner `to` taking links: a link opened sooner is
/// refused before kiln's relay passes a frame on.
fn reach_ready(dir: &Path, from: &str, to: &str) -> anyhow::Result<()> {
    use kiln::reach::local::{Client, Ev, Req};
    let asked = || -> anyhow::Result<Option<()>> {
        let mut c = Client::connect(&kiln::reach::local::path(from, &dir.join(format!("{from}-state"))))?;
        c.send(&Req::Runners { want: Some(to.to_owned()) })?;
        loop {
            match c.event()? {
                Some(Ev::Runners(json)) => {
                    let v: Value = serde_json::from_str(&json)?;
                    let named = format!("\"{to}\"");
                    return Ok(v.get("runners").and_then(Value::as_array).into_iter().flatten().any(|c| c.to_string().contains(&named)).then_some(()));
                }
                Some(Ev::Error(_)) | None => return Ok(None),
                Some(_) => {}
            }
        }
    };
    until(Duration::from_secs(90), || Ok(asked().ok().flatten())).map_err(|e| anyhow::anyhow!("{to} taking {from}'s reach links: {e}: {}", reach_said(dir)))
}

/// What the server and the runner processes in `dir` logged about reach links and relays: why a
/// test of them didn't go through.
fn reach_said(dir: &Path) -> String {
    let mut names: Vec<std::path::PathBuf> = std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "log")).collect()).unwrap_or_default();
    names.sort();
    let logs: Vec<String> = names
        .iter()
        .map(|n| {
            let text = std::fs::read_to_string(n).unwrap_or_default();
            // Not a busy receiver's port, said every 5 s.
            let lines: Vec<&str> = text.lines().filter(|l| ["reach", "relay", "frames", "custody", "panicked", "WARN", "ERROR"].iter().any(|w| l.contains(w)) && !l.contains("port is held")).collect();
            format!("{}:\n{}", n.display(), lines.get(lines.len().saturating_sub(80)..).unwrap_or(&[]).join("\n"))
        })
        .collect();
    logs.join("\n")
}

/// Asks kiln `name` as agent session `id`'s MCP server asks it, through runner `runner`'s socket
/// (in `dir`): its answer, or why not.
fn ask_as(dir: &Path, runner: &str, id: &str, name: &str, input: Value) -> anyhow::Result<String> {
    use kiln::reach::local::{Client, Ev, Req};
    let mut c = Client::connect(&kiln::reach::local::path(runner, &dir.join(format!("{runner}-state"))))?;
    c.send(&Req::Ask { json: json!({"op": "call", "session": id, "name": name, "input": input}).to_string() })?;
    Ok(match c.event()? {
        Some(Ev::Answer(a)) => a,
        Some(Ev::Error(e)) => e,
        other => format!("{other:?}"),
    })
}

/// A runner `name` connected to the server at `port`, its own identity certified, with a session
/// open on it whose acts are signed (its identity at its first epoch certified): the runner and
/// the session.
fn signing_session(dir: &Path, port: u16, admin: &str, name: &str, chaos: Option<&str>) -> anyhow::Result<(agents::Runner, String)> {
    let (r, log) = agents::runner_with(dir, port, name, chaos, TALK, &[])?;
    agents::connected(port, admin, name, &log, Duration::from_secs(120))?;
    until(Duration::from_secs(60), || {
        let (st, body) = http(port, "GET", "/api/identities?kind=runner", admin, None)?;
        anyhow::ensure!(st == 200, "{st} {body}");
        let v: Value = serde_json::from_str(&body)?;
        let mark = on(name);
        Ok(v.get("identities").and_then(Value::as_array).is_some_and(|l| l.iter().any(|i| field(i, "subject").contains(&mark) && !field(i, "verified").is_empty())).then_some(()))
    })?;
    let id = agents::open_session(port, admin, name, &dir.join("work-here"), Duration::from_secs(60))?;
    first_epoch(port, admin, &id)?;
    Ok((r, id))
}

/// W27-R08's relays (the Transfers plan's step 13): a runner relays its agent's calls to kiln. Each
/// act goes with the runner's record of passing it on, numbered on its stream, and kiln's receipt
/// follows; a call's chain walks from kiln's receipt to the runner's record, which the agent's own
/// act began. No call passed on without its record: the stream holds one per call, none missing,
/// none unanswered.
#[test]
fn w27_r08_relays_a_runners_relay_of_its_agents_calls() -> anyhow::Result<()> {
    const CALLS: u64 = 4;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
    let (_a, id) = signing_session(dir.path(), port, admin, "box-a", None)?;
    for _ in 0..CALLS {
        let a = ask_as(dir.path(), "box-a", &id, "goals.get", json!({"agent": id}))?;
        anyhow::ensure!(!a.contains("\"error\""), "the call answered: {a}");
    }
    let v = until(Duration::from_secs(60), || {
        let v = relays(port, admin, 0)?;
        Ok((stream_counts(&v, &on("box-a"), "calls to kiln") == (CALLS, 0) && v.get("unanswered").and_then(Value::as_array).is_some_and(Vec::is_empty)).then_some(v))
    })
    .map_err(|e| anyhow::anyhow!("{e}: {}", relays(port, admin, 0).unwrap_or_default()))?;
    assert!(v.get("unanswered").and_then(Value::as_array).is_some_and(Vec::is_empty), "each answered by kiln's receipt: {v}");
    let receipts = receipts_by(dir.path(), "/server")?;
    assert_eq!(u64::try_from(receipts.len())?, CALLS, "a receipt of kiln's for each call");
    let hops = walked_hops(port, admin, receipts.first().map(String::as_str).unwrap_or_default())?;
    assert!(matches!(hops.as_slice(), [(r, server), (p, runner)] if r == "received" && server.ends_with("/server") && p == "passed-on" && runner.contains(&on("box-a"))), "{hops:?}");
    let (server, runner) = (std::fs::read_to_string(dir.path().join("server.log")).unwrap_or_default(), std::fs::read_to_string(dir.path().join("box-a.log")).unwrap_or_default());
    assert!(!server.contains("passed this act on with no record") && !runner.contains("passed on to kiln unrecorded"), "no act relayed without its record");
    assert_eq!(relay_breaks(port, admin, &on("box-a"))?, Vec::<Value>::new(), "each passed on unchanged");
    // Walked as `kiln custody walk` walks it (the Transfers plan's step 15): a call's chain, and with
    // one hop's record removed, then changed where kiln keeps it, the break named at that hop.
    common::custody::walked_and_broken(port, admin, &dir.path().join("data/kiln.db"), receipts.first().map(String::as_str).unwrap_or_default())?;
    Ok(())
}

/// W27-R08's relays: runners that drop, swap or alter every second act of their agents' they pass on
/// (test builds' faults). kiln refuses the altered and the swapped, saying why, and records each as
/// a break naming the runner; a dropped one never comes, and the next shows the gap, a break naming
/// the runner too, which a walk through that runner's stream says again.
#[cfg(feature = "chaos")]
#[test]
fn w27_r08_relays_that_drop_swap_or_alter_an_agents_call_are_caught() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
    for fault in ["alter", "swap", "drop"] {
        let name = format!("box-{fault}");
        let (_r, id) = signing_session(dir.path(), port, admin, &name, Some(&format!("relay_calls={fault}")))?;
        // Each act another than the one before (a swap of the same act would be none): calls 2 and
        // 4 are agents.get, between goals.gets.
        let op = |i: usize| if i.is_multiple_of(2) { "goals.get" } else { "agents.get" };
        let answers: Vec<String> = (0..5).map(|i| ask_as(dir.path(), &name, &id, op(i), json!({"agent": id}))).collect::<anyhow::Result<_>>()?;
        let refused: Vec<&String> = answers.iter().filter(|a| a.contains("refused")).collect();
        let relay = on(&name);
        let breaks = until(Duration::from_secs(60), || {
            let b = relay_breaks(port, admin, &relay)?;
            Ok((b.len() >= 2).then_some(b))
        })
        .map_err(|e| anyhow::anyhow!("{fault}: {e}: answers {answers:?}"))?;
        let kinds: Vec<&str> = breaks.iter().map(|b| field(b, "kind")).collect();
        match fault {
            "alter" | "swap" => {
                assert_eq!(refused.len(), 2, "{fault}: calls 2 and 4 refused where kiln took them: {answers:?}");
                let kind = if fault == "alter" { "changed" } else { "swapped" };
                assert!(kinds.iter().filter(|k| **k == kind).count() >= 2, "{fault}: {kinds:?}");
                assert!(breaks.iter().filter(|b| field(b, "kind") == kind).all(|b| field(b, "refused").contains("agents.get")), "{fault}: what was refused named: {breaks:?}");
            }
            _ => {
                assert!(answers.get(1).is_some_and(|a| a.contains("dropped")) && answers.get(3).is_some_and(|a| a.contains("dropped")), "drop: calls 2 and 4 never reached kiln: {answers:?}");
                assert!(answers.get(2).is_some_and(|a| !a.contains("error")), "drop: the call after a gap is taken: {answers:?}");
                assert!(kinds.iter().filter(|k| **k == "dropped").count() >= 2, "drop: {kinds:?}");
                assert!(breaks.iter().any(|b| field(b, "detail").contains("record 2 on calls to kiln") && field(b, "detail").contains("never came")), "drop: {breaks:?}");
                // A walk through that stream says so too: its records are numbered with a gap.
                let v = relays(port, admin, 3600)?;
                let (records, missing) = stream_counts(&v, &relay, "calls to kiln");
                assert_eq!((records, missing), (3, 2), "drop: 3 records kept, 2 numbers never came: {v}");
            }
        }
    }
    Ok(())
}

/// W27-R08's relays: kiln relays one agent's words to another's runner. The sender's act passes
/// through unchanged, signed end to end: its runner records passing it on to kiln, kiln records
/// passing it on to the recipient's runner process, numbered on its stream to it, and that process
/// checks the act and both records before its session takes the words, then signs its receipt. The
/// words' chain walks from that receipt through both relays.
#[test]
fn w27_r08_relays_kilns_relay_of_an_agents_words() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
    let (_a, a) = signing_session(dir.path(), port, admin, "box-a", None)?;
    let (_b, b) = signing_session(dir.path(), port, admin, "box-b", None)?;
    let said = ask_as(dir.path(), "box-a", &a, "agents.send", json!({"agent": b, "text": "the build is green"}))?;
    anyhow::ensure!(said.contains("\"sent\":true"), "{said}");
    let receipt = until(Duration::from_secs(60), || Ok(receipts_by(dir.path(), &on("box-b"))?.into_iter().next()))?;
    let hops = walked_hops(port, admin, &receipt)?;
    let kinds: Vec<(&str, bool)> = hops.iter().map(|(k, c)| (k.as_str(), c.ends_with("/server"))).collect();
    assert_eq!(kinds, vec![("received", false), ("passed-on", true), ("passed-on", false)], "box-b's receipt, kiln's record, box-a's: {hops:?}");
    assert!(hops.first().is_some_and(|(_, c)| c.contains(&on("box-b"))) && hops.get(2).is_some_and(|(_, c)| c.contains(&on("box-a"))), "{hops:?}");
    let v = relays(port, admin, 0)?;
    assert_eq!(stream_counts(&v, "/server", "words to "), (1, 0), "kiln's record of the words, numbered: {v}");
    assert!(v.get("unanswered").and_then(Value::as_array).is_some_and(Vec::is_empty), "{v}");
    assert_eq!(relay_breaks(port, admin, "kiln://")?, Vec::<Value>::new(), "each passed on unchanged");
    // Walked as `kiln custody walk` walks it (step 15): the words' chain through both relays, and with
    // one hop's record removed, then changed where kiln keeps it, the break named at that hop.
    common::custody::walked_and_broken(port, admin, &dir.path().join("data/kiln.db"), &receipt)?;
    Ok(())
}

/// W27-R08's relays: a faulty relay of agents' words at kiln (test builds' faults): every second
/// message's words changed after kiln recorded passing them on, another's swapped in, or dropped.
/// The changed and the swapped are refused by the recipient's runner, its session never given
/// them, and the break recorded naming kiln's server; the dropped is kiln's record no receipt
/// follows, recorded so.
#[cfg(feature = "chaos")]
#[test]
fn w27_r08_relays_that_drop_swap_or_alter_an_agents_words_are_caught() -> anyhow::Result<()> {
    for fault in ["alter", "swap", "drop"] {
        let dir = tempfile::tempdir()?;
        let spec = format!("relay_words={fault}");
        let s = common::start_env(dir.path(), &Options::default(), false, &[("KILN_CHAOS", spec.as_str())])?;
        let (port, admin) = (s.port, s.admin.as_str());
        let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
        let (_a, a) = signing_session(dir.path(), port, admin, "box-a", None)?;
        let (_b, b) = signing_session(dir.path(), port, admin, "box-b", None)?;
        let first = ask_as(dir.path(), "box-a", &a, "agents.send", json!({"agent": b, "text": "the build is green"}))?;
        anyhow::ensure!(first.contains("\"sent\":true"), "{fault}: {first}");
        until(Duration::from_secs(60), || Ok(receipts_by(dir.path(), &on("box-b"))?.into_iter().next()))?;
        let second = ask_as(dir.path(), "box-a", &a, "agents.send", json!({"agent": b, "text": "the deploy is done"}))?;
        // A while for the recipient's receipt or its word of a break; then kiln's records no
        // receipt answers are recorded as breaks, as kiln does once they lapse.
        std::thread::sleep(Duration::from_secs(15));
        let (st, body) = http(port, "POST", "/api/custody/relays?lapse=10", admin, None)?;
        anyhow::ensure!(st == 200, "{st} {body}");
        let breaks = until(Duration::from_secs(60), || {
            let b = relay_breaks(port, admin, "/server")?;
            Ok((!b.is_empty()).then_some(b))
        })
        .map_err(|e| anyhow::anyhow!("{fault}: {e}: the second said {second}"))?;
        let kind = match fault {
            "alter" => "changed",
            "swap" => "swapped",
            _ => "no-receipt",
        };
        assert!(breaks.iter().any(|b| field(b, "kind") == kind), "{fault}: a break naming kiln's server: {breaks:?}");
        if fault == "drop" {
            assert!(second.contains("\"sent\":true"), "dropped on the way, as said sent: {second}");
        } else {
            assert!(second.contains("isn't valid") && !second.contains("\"sent\":true"), "{fault}: refused where its session would take it: {second}");
            assert!(breaks.iter().any(|b| field(b, "kind") == kind && field(b, "refused").contains("message ") && field(b, "detail").contains("runner box-b")), "{fault}: {breaks:?}");
        }
    }
    Ok(())
}

/// W27-R08's relays: kiln relays a reach link's frames between two runners, unread: it records
/// each run of them by the chain of their ciphertext's digests, numbered on its stream to each end,
/// and each end answers with its receipt of the frames as they came. Every frame passed on is in a
/// run kiln recorded, each answered; a run's chain walks from the receipt to kiln's record.
#[test]
fn w27_r08_relays_kilns_relay_of_a_reach_links_frames() -> anyhow::Result<()> {
    use kiln::reach::local::{Client, Ev, Req, RunReq};
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let (_a, log_a) = agents::runner_with(dir.path(), port, "box-a", None, TALK, &[])?;
    let (_b, log_b) = agents::runner_with(dir.path(), port, "box-b", None, TALK, &[])?;
    for (name, log) in [("box-a", &log_a), ("box-b", &log_b)] {
        agents::connected(port, admin, name, log, Duration::from_secs(120))?;
    }
    reach_ready(dir.path(), "box-a", "box-b")?;
    let ran = until(Duration::from_secs(90), || {
        let mut c = Client::connect(&kiln::reach::local::path("box-a", &dir.path().join("box-a-state")))?;
        c.send(&Req::Run(Box::new(RunReq { target: "box-b".into(), command: "echo relayed by kiln".into(), ..RunReq::default() })))?;
        let mut out = Vec::new();
        loop {
            match c.event()? {
                Some(Ev::Exit { .. }) => return Ok(Some(String::from_utf8_lossy(&out).into_owned())),
                Some(Ev::Output { data, .. }) => out.extend(data),
                Some(Ev::Error(_)) | None => return Ok(None),
                Some(_) => {}
            }
        }
    })?;
    assert!(ran.contains("relayed by kiln"), "{ran}");
    // kiln records a run within 10 s of its first frame; each end's receipt comes at its next beat.
    // A run recorded since may be a beat ahead of its receipt: those 20 s old or more are answered.
    until(Duration::from_secs(90), || Ok((!receipts_by(dir.path(), &on("box-b"))?.is_empty() && !receipts_by(dir.path(), &on("box-a"))?.is_empty()).then_some(())))
        .map_err(|e| anyhow::anyhow!("{e}: each end's receipt: {}", relays(port, admin, 20).unwrap_or_default()))?;
    let v = until(Duration::from_secs(90), || {
        let v = relays(port, admin, 20)?;
        let (to_b, to_a) = (stream_counts(&v, "/server", "reach link "), v.get("unanswered").and_then(Value::as_array).map_or(0, Vec::len));
        Ok((to_b.0 >= 2 && to_b.1 == 0 && to_a == 0).then_some(v))
    })
    .map_err(|e| anyhow::anyhow!("{e}: {}", relays(port, admin, 20).unwrap_or_default()))?;
    let streams: Vec<&str> = v.get("streams").and_then(Value::as_array).into_iter().flatten().map(|s| field(s, "stream")).filter(|s| s.starts_with("reach link ")).collect();
    assert!(streams.iter().any(|s| s.contains(&on("box-a"))) && streams.iter().any(|s| s.contains(&on("box-b"))), "a stream to each end: {streams:?}");
    let receipt = receipts_by(dir.path(), &on("box-b"))?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no receipt by box-b"))?;
    let hops = walked_hops(port, admin, &receipt).map_err(|e| anyhow::anyhow!("{e}: {}", reach_said(dir.path())))?;
    assert!(matches!(hops.as_slice(), [(r, b), (p, server)] if r == "received" && b.contains(&on("box-b")) && p == "passed-on" && server.ends_with("/server")), "{hops:?}");
    // Each run's receipt names the frames as kiln's relay passed them on: no break.
    assert_eq!(relay_breaks(port, admin, "/server")?, Vec::<Value>::new(), "each passed on unchanged");
    Ok(())
}

/// W27-R08's relays: kiln's relay of a reach link's frames, faulty (a test build's: frames 2, 4 …
/// each way dropped, swapped or altered). The link's end can't read them, so the run fails there;
/// and its receipt of the frames as they came differs from kiln's record of them, a break recorded
/// naming kiln's server.
#[cfg(feature = "chaos")]
#[test]
fn w27_r08_relays_that_drop_swap_or_alter_a_reach_links_frames_are_caught() -> anyhow::Result<()> {
    use kiln::reach::local::{Client, Ev, Req, RunReq};
    for fault in ["alter", "swap", "drop"] {
        let dir = tempfile::tempdir()?;
        let spec = format!("relay_frames={fault}");
        let s = common::start_env(dir.path(), &Options::default(), false, &[("KILN_CHAOS", spec.as_str())])?;
        let (port, admin) = (s.port, s.admin.as_str());
        let (_a, log_a) = agents::runner_with(dir.path(), port, "box-a", None, TALK, &[])?;
        let (_b, log_b) = agents::runner_with(dir.path(), port, "box-b", None, TALK, &[])?;
        for (name, log) in [("box-a", &log_a), ("box-b", &log_b)] {
            agents::connected(port, admin, name, log, Duration::from_secs(120))?;
        }
        reach_ready(dir.path(), "box-a", "box-b")?;
        // A run whose link failed before kiln's relay passed a frame of it on (refused on the way)
        // goes again: kiln records a run of frames within 10 s of its first, and its last as the
        // link closes.
        for tries in 1.. {
            let mut c = Client::connect(&kiln::reach::local::path("box-a", &dir.path().join("box-a-state")))?;
            c.send(&Req::Run(Box::new(RunReq { target: "box-b".into(), command: "echo relayed by kiln".into(), ..RunReq::default() })))?;
            // Its events, on their own: a link that stalls (a frame dropped) ends only as its
            // handshake is given up.
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let exited = loop {
                    match c.event() {
                        Ok(Some(Ev::Exit { .. })) => break true,
                        Ok(Some(Ev::Error(_)) | None) | Err(_) => break false,
                        Ok(Some(_)) => {}
                    }
                };
                let _ = tx.send(exited);
            });
            let exited = rx.recv_timeout(Duration::from_secs(240)).unwrap_or(false);
            assert!(!exited, "{fault}: a link whose frames changed on the way can't be read: the run never went");
            let passed = until(Duration::from_secs(40), || Ok(relays(port, admin, 0).ok().filter(|v| stream_counts(v, "/server", "reach link ").0 > 0)));
            if passed.is_ok() {
                break;
            }
            anyhow::ensure!(tries < 3, "{fault}: kiln's relay passed no frame of {tries} runs' links on: {}", reach_said(dir.path()));
        }
        let breaks = until(Duration::from_secs(90), || {
            let (st, body) = http(port, "POST", "/api/custody/relays?lapse=20", admin, None)?;
            anyhow::ensure!(st == 200, "{st} {body}");
            let b = relay_breaks(port, admin, "/server")?;
            Ok(b.iter().any(|b| matches!(field(b, "kind"), "changed" | "no-receipt")).then_some(b))
        })
        .map_err(|e| {
            let (_, all) = http(port, "GET", "/api/custody/breaks", admin, None).unwrap_or_default();
            anyhow::anyhow!("{fault}: {e}: relays {}; breaks {all}; {}", relays(port, admin, 0).unwrap_or_default(), reach_said(dir.path()))
        })?;
        assert!(breaks.iter().any(|b| field(b, "detail").contains("reach link")), "{fault}: the break names the link's frames: {breaks:?}");
    }
    Ok(())
}

/// W27-R08's relays: blue/green's old build forwards kiln's frames to the new one. Another agent's
/// words keep coming to a session as its runner is upgraded; those that reach the old build once it
/// has handed everything over go on to the new build, each with the old build's record of passing
/// the sender's act on (following kiln's record of it), numbered on its stream to the new build,
/// which checks the act and every record before its session takes the words, and signs its
/// receipt. In a test build the runner tells kiln everything three seconds late (`delay`), so kiln
/// goes on sending the words to the old build a while after it hands the session over, whatever the
/// timing. Every frame of signed words forwarded has its record, none missing, each answered. With
/// the old build faulty (a test build's `relay_forward`: every second such frame dropped,
/// swapped for the one before, or altered), the new build refuses each not as recorded, and sees
/// the gap a dropped one leaves; kiln records each break naming the old build.
#[test]
#[cfg_attr(debug_assertions, ignore = "needs a release build: an upgrade downloads the server's own kiln, and a debug one is larger than kiln serves (updates::MAX_BINARY)")]
fn w27_r08_relays_blue_greens_forwarder() -> anyhow::Result<()> {
    let faults: &[Option<&str>] = if cfg!(feature = "chaos") { &[None, Some("relay_forward=alter"), Some("relay_forward=swap"), Some("relay_forward=drop")] } else { &[None] };
    for fault in faults {
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
        let s = start(dir.path(), &Options::default())?;
        let (port, admin) = (s.port, s.admin.as_str());
        let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
        let before_build = dir.path().join("kiln-before");
        let mut bytes = std::fs::read(env!("CARGO_BIN_EXE_kiln"))?;
        bytes.push(0);
        std::fs::write(&before_build, bytes)?;
        std::fs::set_permissions(&before_build, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
        let spec = [cfg!(feature = "chaos").then_some("delay=3s"), *fault].into_iter().flatten().collect::<Vec<_>>().join(",");
        let env: Vec<(&str, &str)> = (!spec.is_empty()).then_some(("KILN_CHAOS", spec.as_str())).into_iter().collect();
        let (_box_a, log) = supervised(dir.path(), port, "box-a", &before_build, &env)?;
        agents::connected(port, admin, "box-a", &log, Duration::from_secs(60))?;
        let a = agents::open_session(port, admin, "box-a", &dir.path().join("work-here"), Duration::from_secs(60))?;
        first_epoch(port, admin, &a)?;
        let (_box_b, b) = signing_session(dir.path(), port, admin, "box-b", None)?;
        // b's words to a, from eight senders at once (each waits for kiln to deliver its words), every
        // tenth of a second, while a's runner is upgraded.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let words: Vec<_> = (0..8u32)
            .map(|t| {
                let (sending, home, (to, from)) = (std::sync::Arc::clone(&stop), dir.path().to_path_buf(), (a.clone(), b.clone()));
                std::thread::spawn(move || {
                    let mut n = 0u32;
                    while !sending.load(std::sync::atomic::Ordering::Relaxed) {
                        n += 1;
                        let _ = ask_as(&home, "box-b", &from, "agents.send", json!({"agent": to, "text": format!("message {t}.{n} as its runner is upgraded")}));
                        std::thread::sleep(Duration::from_millis(100));
                    }
                })
            })
            .collect();
        let (st, body) = http(port, "POST", "/api/updates/runners/box-a", admin, None)?;
        anyhow::ensure!(st == 202, "{st} {body}");
        let job = serde_json::from_str::<Value>(&body)?.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let done = until(Duration::from_secs(300), || {
            let (_, j) = http(port, "GET", &format!("/api/updates/jobs/{job}"), admin, None)?;
            let v: Value = serde_json::from_str(&j).unwrap_or_default();
            Ok(matches!(field(&v, "state"), "done" | "failed" | "succeeded").then_some(v))
        });
        std::thread::sleep(Duration::from_secs(5));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for w in words {
            let _ = w.join();
        }
        done.map_err(|e| anyhow::anyhow!("{fault:?}: the upgrade: {e}"))?;
        let blue = format!("{}instance/main", on("box-a"));
        let v = until(Duration::from_secs(90), || {
            let v = relays(port, admin, 20)?;
            let (n, missing) = stream_counts(&v, blue, "frames forwarded to ");
            let unanswered = v.get("unanswered").and_then(Value::as_array).map_or(0, |u| u.iter().filter(|r| field(r, "relay").contains(blue)).count());
            // A faulty forwarder's refused records never reach kiln's streams: their breaks are
            // checked below.
            Ok((n >= 2 && (fault.is_some() || (missing == 0 && unanswered == 0))).then_some(v))
        })
        .map_err(|e| anyhow::anyhow!("{fault:?}: {e}: {}", relays(port, admin, 0).unwrap_or_default()))?;
        let (forwarded, _) = stream_counts(&v, blue, "frames forwarded to ");
        match fault {
            None => {
                let receipts = receipts_by(dir.path(), &on("box-a"))?.into_iter().filter(|r| walked_hops(port, admin, r).is_ok_and(|h| h.get(1).is_some_and(|(_, c)| c.ends_with(&blue)))).count();
                assert_eq!(u64::try_from(receipts)?, forwarded, "the new build's receipt of each frame of words forwarded: {v}");
                assert_eq!(relay_breaks(port, admin, blue)?, Vec::<Value>::new(), "each passed on unchanged");
            }
            Some(f) => {
                let breaks = until(Duration::from_secs(60), || {
                    let b = relay_breaks(port, admin, blue)?;
                    Ok((!b.is_empty()).then_some(b))
                })?;
                let kind = if f.ends_with("drop") {
                    "dropped"
                } else if f.ends_with("swap") {
                    "swapped"
                } else {
                    "changed"
                };
                assert!(breaks.iter().any(|b| field(b, "kind") == kind), "{f}: {breaks:?}");
            }
        }
    }
    Ok(())
}

/// A chain's hops as (kind, custodian, whom it's held for, the delegation it's under).
#[cfg(feature = "chaos")]
fn hops_held(v: &Value) -> Vec<(String, String, String, String)> {
    v.get("hops").and_then(Value::as_array).into_iter().flatten().map(|h| (field(h, "kind").to_owned(), field(h, "custodian").to_owned(), field(h, "for").to_owned(), field(h, "under").to_owned())).collect()
}

/// [`w27_r08_custody_between_machines`]'s kiln run's sync and reach run (the Transfers plan's step
/// 12, its example), on the runners it started: kiln run on box-b for an agent session on box-c. The session hands its
/// workspace (its folder's tree, by `kiln run --reuse`'s digest) and the run on to box-b's runner
/// process, with a delegation to receive both for it; box-b receives each for the session under
/// that delegation, and hands the run's output back, which the session receives: every hop signed
/// by both sides. Then box-d, whose sync changes a file on its way (its `sync_file_changed`), runs on
/// box-b: box-b refuses the copy before anything runs in it, and kiln records the break at that hop
/// and the refusal.
#[cfg(feature = "chaos")]
fn synced_runs(dir: &Path, port: u16, admin: &str) -> anyhow::Result<()> {
    use kiln::reach::local::{Client, Ev, Req, RunReq};
    let cwd = dir.join("work-c");
    let home = dir.join("home");
    let _harnesses = Harnesses::new(&cwd, &home)?;
    let id = agents::open_session(port, admin, "box-c", &cwd, Duration::from_secs(60))?;
    let epoch = first_epoch(port, admin, &id)?;
    let harness: i64 = until(Duration::from_secs(60), || {
        let said = std::fs::read_to_string(cwd.join(".fake-claude-messages"))?;
        Ok(said.lines().find_map(|l| serde_json::from_str::<Value>(l).ok()?.get("pid")?.as_i64()))
    })?;
    // The session's workspace, a repository in its folder, and its run on box-b, as the session's
    // own child.
    let repo = cwd.join("app");
    std::fs::create_dir_all(&repo)?;
    let said = "built on box-b for the session\n";
    std::fs::write(repo.join("data.txt"), said)?;
    for args in [&["init", "-q"][..], &["add", "."], &["commit", "-qm", "one"]] {
        git(&home, &repo, args)?;
    }
    let tree = kiln::reach::reuse::tree(&repo)?;
    std::fs::write(cwd.join(format!(".fake-claude-acts-{harness}")), format!("cd {} && {} run --on box-b -- cat data.txt\n", repo.display(), env!("CARGO_BIN_EXE_kiln")))?;
    let acted = until(Duration::from_secs(120), || Ok(std::fs::read_to_string(cwd.join(".fake-claude-acted")).ok().filter(|t| !t.is_empty())))?;
    let run: Value = serde_json::from_str(acted.lines().next().unwrap_or_default())?;
    assert!(run.get("code") == Some(&json!(0)) && field(&run, "out").contains(said.trim_end()), "the session's run on box-b: {run}");
    let chain_of = |d: &str| -> anyhow::Result<Value> {
        let (_, body) = http(port, "GET", &format!("/api/custody?digest=sha256:{d}"), admin, None)?;
        Ok(serde_json::from_str(&body).unwrap_or_default())
    };
    let unbroken = |d: &str, n: usize| {
        until(Duration::from_secs(90), || {
            let v = chain_of(d)?;
            Ok((v.get("unbroken") == Some(&json!(true)) && hops_held(&v).len() == n).then_some(v))
        })
        .map_err(|e| anyhow::anyhow!("{e}: {}\n{}", chain_of(d).unwrap_or_default(), said_of(dir, port, admin, &id)))
    };
    let session = format!("/agent/{id}/epoch/{epoch}");
    let (on_c, on_b) = (format!("{session}{}", on("box-c")), on("box-b"));
    // The workspace: the session's hand-on on box-c, box-b's receipt for it under its delegation.
    let ws = unbroken(&tree, 2)?;
    match hops_held(&ws).as_slice() {
        [(r, taker, held, under), (h, giver, _, _)] if r == "received" && taker.contains(&on_b) && held.ends_with(&session) && !under.is_empty() && h == "handed-on" && giver.contains(&on_c) => {}
        other => anyhow::bail!("box-b's receipt of the session's workspace, for it under its delegation: {other:?}"),
    }
    assert!(half_named(&ws) == 0, "each hop names both sides, signed: {ws}");
    // The run and its output, walked back from the output the session received.
    let out = unbroken(&kiln::util::sha256_hex(said.as_bytes()), 4)?;
    match hops_held(&out).as_slice() {
        [(r_out, s1, _, _), (h_out, b1, _, _), (r_run, b2, held, under), (h_run, s2, _, _)]
            if (r_out.as_str(), h_out.as_str(), r_run.as_str(), h_run.as_str()) == ("received", "handed-on", "received", "handed-on")
                && s1.contains(&on_c)
                && b1.contains(&on_b)
                && b2.contains(&on_b)
                && held.ends_with(&session)
                && !under.is_empty()
                && s2.contains(&on_c) => {}
        other => anyhow::bail!("the session's run on box-b and its output back: {other:?}"),
    }
    assert!(half_named(&out) == 0, "each hop names both sides, signed: {out}");
    // A synced file changed on its way: refused on box-b before anything ran, the break recorded
    // at that hop, and the refusal.
    let other = dir.join("work-d");
    std::fs::create_dir_all(&other)?;
    std::fs::write(other.join("data.txt"), "box-d's tree\n")?;
    for args in [&["init", "-q"][..], &["add", "."], &["commit", "-qm", "one"]] {
        git(&home, &other, args)?;
    }
    let mut c = Client::connect(&kiln::reach::local::path("box-d", &dir.join("box-d-state")))?;
    c.send(&Req::Run(Box::new(RunReq { target: "box-b".into(), command: "cat data.txt".into(), cwd: Some(other.to_string_lossy().into_owned()), sync: true, ..RunReq::default() })))?;
    let mut evs = Vec::new();
    while let Some(e) = c.event()? {
        let end = matches!(e, Ev::Exit { .. } | Ev::Error(_));
        evs.push(e);
        if end {
            break;
        }
    }
    assert!(evs.iter().any(|e| matches!(e, Ev::Error(why) if why.contains("refused"))) && !evs.iter().any(|e| matches!(e, Ev::Output { .. })), "the changed copy is refused before anything runs: {evs:?}");
    let broke = until(Duration::from_secs(90), || {
        let (_, body) = http(port, "GET", "/api/custody/breaks", admin, None)?;
        let breaks: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("breaks").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(breaks.into_iter().find(|b| field(b, "kind") == "changed" && field(b, "detail").contains("workspace ")))
    })
    .map_err(|e| anyhow::anyhow!("{e}: {}", said_of(dir, port, admin, &id)))?;
    let db = rusqlite::Connection::open(dir.join("data/kiln.db"))?;
    let custodian = |d: &str| -> anyhow::Result<String> { Ok(db.query_row("SELECT custodian FROM custody WHERE digest = ?1", [d], |r| r.get(0))?) };
    let (record, follows) = (field(&broke, "record"), broke.pointer("/around/1").and_then(Value::as_str).unwrap_or_default());
    let (took, gave) = (custodian(record)?, custodian(follows)?);
    assert!(took.contains(&on_b) && gave.contains(&on("box-d")), "box-b's receipt of what came, box-d's hand-on of what it sent: {broke}");
    let refused = until(Duration::from_secs(60), || {
        let (_, body) = http(port, "GET", "/api/transfers/refused", admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        Ok(v.get("refused").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().find(|r| field(r, "act") == "a hand-over between machines" && field(r, "why").contains("box-d")))
    })?;
    assert!(field(&refused, "came_from").starts_with("runner box-b"), "box-b recorded what it refused: {refused}");
    Ok(())
}

/// kiln's store of bytes: two binaries for other platforms uploaded, each kept in the store and
/// downloaded whole. One walks unbroken: kiln's hand-on to the store and its receipt for it, kiln's
/// take for the store and its receipt of what it read. The other the server's `blob_changed`
/// changes as it's read: its download ends short, and the break is recorded where kiln received
/// other bytes than it took for the store.
#[cfg(feature = "chaos")]
fn store_of_bytes(dir: &Path, port: u16, admin: &str) -> anyhow::Result<()> {
    let binary = |seed: u32| -> Vec<u8> {
        let mut b = b"\x7fELF".to_vec();
        b.extend((0..(2u32 << 20)).map(|i| ((i.wrapping_add(seed).wrapping_mul(2_654_435_761)) >> 24) as u8));
        b
    };
    let headers = format!("Authorization: Bearer {admin}\r\nContent-Type: application/octet-stream\r\n");
    let (kept, changed) = ((binary(1), "aarch64-apple-darwin"), (binary(2), CHANGED_BINARY));
    for (bytes, target) in [&kept, &changed] {
        let (st, body) = common::raw(port, "POST", &format!("/api/updates/binaries?target={target}"), &headers, bytes)?;
        anyhow::ensure!(st == 200, "the upload for {target}: {st} {}", String::from_utf8_lossy(&body));
    }
    let (st, got) = common::raw(port, "GET", &format!("/dl/kiln/{}", kept.1), "", b"")?;
    anyhow::ensure!(st == 200 && got == kept.0, "{}: {st}, {} bytes", kept.1, got.len());
    let store = |v: &Value| hops_of(v).get(1).map(|h| h.1.clone()).unwrap_or_default();
    let v = until(Duration::from_secs(30), || {
        let v = chain_of_digest(port, admin, &format!("sha256:{}", kiln::util::sha256_hex(&kept.0)))?;
        Ok((v.get("unbroken") == Some(&json!(true)) && hops_of(&v).len() == 4).then_some(v))
    })
    .map_err(|e| anyhow::anyhow!("{e}: {}", chain_of_digest(port, admin, &format!("sha256:{}", kiln::util::sha256_hex(&kept.0))).unwrap_or_default()))?;
    let kinds: Vec<String> = hops_of(&v).into_iter().map(|h| h.0).collect();
    assert_eq!(kinds, ["received", "taken", "received", "handed-on"], "{v}");
    assert!(store(&v).contains("/blobs/"), "kiln for its store of bytes: {v}");
    assert!(half_named(&v) == 0, "{v}");
    // Changed as it's read: the download ends short, and the break is recorded at the take.
    let short = common::raw(port, "GET", &format!("/dl/kiln/{}", changed.1), "", b"");
    assert!(short.as_ref().map_or(true, |(_, b)| *b != changed.0), "a changed binary is never served whole");
    let broke = until(Duration::from_secs(30), || Ok(breaks_now(port, admin)?.into_iter().find(|b| field(b, "kind") == "changed" && field(b, "detail").contains(&format!("object binaries/kiln-{}", changed.1)))))?;
    let server = custodian_of(dir, field(&broke, "record"))?;
    assert!(server.ends_with("/server"), "kiln's receipt of what it read: {broke}");
    Ok(())
}

/// The conversation feed: box-a's and box-b's agent sessions' rows go to the server's store a
/// batch at a time, each signed by both sides: the runner's hand-on of it and kiln's receipt,
/// walking unbroken. box-b's first batch signed for changes on its way (its `feed_changed`): the
/// server refuses it, the break is recorded, and the batch goes again as it was and is taken.
#[cfg(feature = "chaos")]
fn feed_batches(dir: &Path, port: u16, admin: &str, cwd: &Path) -> anyhow::Result<()> {
    let a = agents::open_session(port, admin, "box-a", cwd, Duration::from_secs(60))?;
    agents::open_session(port, admin, "box-b", cwd, Duration::from_secs(60))?;
    let db = rusqlite::Connection::open(dir.join("data/kiln.db"))?;
    // kiln's receipts of runners' batches, and whose hand-on each follows.
    let receipts = || -> anyhow::Result<Vec<(String, String)>> {
        let mut q = db.prepare("SELECT c.digest, h.custodian FROM custody c JOIN custody_things t ON t.record = c.digest JOIN custody_follows f ON f.record = c.digest JOIN custody h ON h.digest = f.follows WHERE t.name LIKE 'conversation store %' AND c.kind = 'received' AND c.custodian LIKE '%/server'")?;
        let rows = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    };
    // Each runner's batch, received as handed on: one of its chains walks unbroken.
    let unbroken = |m: &str| -> anyhow::Result<Option<Value>> {
        for (record, giver) in receipts()? {
            if giver.contains(&on(m)) {
                let v = chain_of_digest(port, admin, &record)?;
                if v.get("unbroken") == Some(&json!(true)) && half_named(&v) == 0 && hops_of(&v).into_iter().map(|h| h.0).collect::<Vec<_>>() == ["received", "handed-on"] {
                    return Ok(Some(v));
                }
            }
        }
        Ok(None)
    };
    until(Duration::from_secs(120), || Ok(unbroken("box-a")?.zip(unbroken("box-b")?))).map_err(|e| anyhow::anyhow!("{e}: {:?}\n{}", receipts(), said_of(dir, port, admin, &a)))?;
    // box-b's batch, changed on its way: refused and recorded, then taken as it was.
    let broke = breaks_now(port, admin)?.into_iter().find(|x| field(x, "kind") == "changed" && field(x, "detail").contains("conversation store"));
    let broke = broke.ok_or_else(|| anyhow::anyhow!("no break recorded for box-b's changed batch"))?;
    let follows = broke.pointer("/around/1").and_then(Value::as_str).unwrap_or_default();
    assert!(custodian_of(dir, follows)?.contains(&on("box-b")), "box-b's hand-on: {broke}");
    Ok(())
}
