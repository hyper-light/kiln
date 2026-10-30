//! Machines by id (docs/machines.md). A runner that joined by its name, before machine ids, moves to
//! its machine id as it starts, with what kiln kept under its name, and is shown by that name; a
//! name people see is one machine's; the server's own runner is known by its machine's id.
//!
//! `cargo test -p kiln --test machines`

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::agents::{self, Runner, Talk};
use common::{Options, http, machine, start};
use serde_json::{Value, json};

/// `kiln agent` of the server at `port` on the machine whose state is `state`, started as `name`
/// (its machine id made from it), with `more` (`--instance <id>`: a runner process an upgrade
/// started): its log.
fn agent(dir: &Path, port: u16, state: &Path, name: &str, log: &str, more: &[&str]) -> anyhow::Result<(Runner, PathBuf)> {
    let home = dir.join("home");
    std::fs::create_dir_all(&home)?;
    let log_path = dir.join(log);
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let child = common::kiln()
        .args(["agent", "--server", &format!("http://127.0.0.1:{port}"), "--name", name, "--no-terminal"])
        .args(more)
        .env("KILN_MACHINE_ID", name)
        .arg("--work-dir")
        .arg(dir.join(format!("{log}-work")))
        .arg("--state-dir")
        .arg(state)
        .env("HOME", &home)
        .env("NO_COLOR", "1")
        .stdout(Stdio::from(out.try_clone()?))
        .stderr(Stdio::from(out))
        .spawn()?;
    Ok((Runner(child), log_path))
}

/// Stops a runner as its service would (SIGTERM), so the server sees it go at once.
fn stop(mut r: Runner) -> anyhow::Result<()> {
    let sent = std::process::Command::new("kill").arg(r.0.id().to_string()).status()?;
    anyhow::ensure!(sent.success(), "couldn't stop the runner");
    r.0.wait()?;
    Ok(())
}

/// Waits until the server's runners are as `holds` says: them.
fn until(port: u16, admin: &str, what: &str, log: &Path, holds: impl Fn(&[Value]) -> bool) -> anyhow::Result<Vec<Value>> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (_, body) = http(port, "GET", "/api/agents", admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let list = v.as_array().cloned().or_else(|| v.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
        if holds(&list) {
            return Ok(list);
        }
        anyhow::ensure!(Instant::now() < deadline, "{what}, never: {body}\n{}", std::fs::read_to_string(log).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// The machine whose state is `state` enrolls as `name`, sending no machine id, as a kiln from
/// before did: known by that name.
fn enrolled_by_name(port: u16, admin: &str, join: &str, state: &Path, name: &str) -> anyhow::Result<()> {
    let (csr, key) = kiln::pki::make_csr(name)?;
    let ask = json!({ "token": join, "name": name, "csr": csr, "hostname": name });
    let (st, body) = http(port, "POST", "/api/agents/enroll", admin, Some(&ask))?;
    anyhow::ensure!(st == 200, "enrolling: {st} {body}");
    let enrolled: Value = serde_json::from_str(&body)?;
    anyhow::ensure!(text(&enrolled, "name") == name && enrolled.get("label").is_none(), "known by its name: {enrolled}");
    std::fs::create_dir_all(state)?;
    for (file, content) in [("agent.pem", text(&enrolled, "cert_pem")), ("agent.key", key.as_str()), ("ca.pem", text(&enrolled, "ca_pem")), ("quic", text(&enrolled, "quic"))] {
        std::fs::write(state.join(file), content)?;
    }
    Ok(())
}

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

fn connected(a: &Value) -> bool {
    a.get("connected").and_then(Value::as_bool) == Some(true)
}

#[test]
fn a_runner_from_before_moves_to_its_machine_id_with_what_was_kept_under_its_name() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();
    let s = start(dir, &Options::default())?;
    let join = std::fs::read_to_string(dir.join("data/join.token"))?.trim().to_owned();
    let state = dir.join("machine");

    enrolled_by_name(s.port, &s.admin, &join, &state, "old-box")?;

    // It has never connected, so kiln has nothing of it to move yet: it's refused, and goes on by
    // its name. Kiln keeps what's set for it under that name: its agent limit, a person's grant.
    let (first, log) = agent(dir, s.port, &state, "old-box", "first.log", &[])?;
    until(s.port, &s.admin, "old-box connected by its name", &log, |l| l.iter().any(|a| text(a, "name") == "old-box" && connected(a)))?;
    assert!(std::fs::read_to_string(&log)?.contains("it goes on by its name"), "it said why it wasn't moved");
    let (st, body) = http(s.port, "PUT", "/api/runners/old-box/agent-limit", &s.admin, Some(&json!({ "max_sessions": 2 })))?;
    anyhow::ensure!(st == 200, "its agent limit: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": "dan", "grants": ["developer", "runners.terminal@runner:old-box"] })))?;
    anyhow::ensure!((200..300).contains(&st), "dan: {st} {body}");
    stop(first)?;
    until(s.port, &s.admin, "old-box gone away", &log, |l| l.iter().any(|a| text(a, "name") == "old-box" && !connected(a)))?;

    // It starts again, and moves to its machine id as it does: known by the id, shown by its name,
    // what was kept under the name with it.
    let (_runner, log) = agent(dir, s.port, &state, "old-box", "runner.log", &[])?;
    let id = machine("old-box");
    let list = until(s.port, &s.admin, "the runner connected by its machine id", &log, |l| l.iter().any(|a| text(a, "name") == id && connected(a)))?;
    let me = list.iter().find(|a| text(a, "name") == id).cloned().unwrap_or_default();
    assert_eq!(text(&me, "label"), "old-box", "shown by its name: {me}");
    assert!(!list.iter().any(|a| text(a, "name") == "old-box"), "nothing is known by its name any more: {list:?}");
    assert!(std::fs::read_to_string(&log)?.contains("this runner is known by its machine id now"), "it said so");
    let (st, body) = http(s.port, "GET", &format!("/api/runners/{id}/agent-limit"), &s.admin, None)?;
    let limit: Value = serde_json::from_str(&body).unwrap_or_default();
    assert!(st == 200 && limit.get("max_sessions") == Some(&json!(2)), "its agent limit went with it: {st} {body}");
    let (st, _) = http(s.port, "GET", "/api/runners/old-box/agent-limit", &s.admin, None)?;
    assert_eq!(st, 404, "nothing is set under its name any more");
    let (_, body) = http(s.port, "GET", "/api/users", &s.admin, None)?;
    let users: Value = serde_json::from_str(&body)?;
    let dan = users.get("users").and_then(Value::as_array).into_iter().flatten().find(|u| text(u, "name") == "dan").cloned().unwrap_or_default();
    let grants: Vec<&str> = dan.get("grants").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let on_it = format!("runners.terminal@runner:{id}");
    assert!(grants.contains(&on_it.as_str()) && !grants.contains(&"runners.terminal@runner:old-box"), "dan's grant went with it: {dan}");

    // Renamed, it's shown by its new name, and that name is its machine's: another machine that
    // asks for it is shown by another.
    let (st, body) = http(s.port, "POST", &format!("/api/agents/{id}/label"), &s.admin, Some(&json!({ "label": "build-box" })))?;
    anyhow::ensure!(st == 200, "renaming it: {st} {body}");
    let (_other, other_log) = agents::runner(dir, s.port, "build-box", None, Talk { lines: 1, every_ms: 10 })?;
    let other = machine("build-box");
    let list = until(s.port, &s.admin, "the other machine connected", &other_log, |l| l.iter().any(|a| text(a, "name") == other && connected(a)))?;
    let shown = |k: &str| list.iter().find(|a| text(a, "name") == k).map(|a| text(a, "label").to_owned()).unwrap_or_default();
    assert_eq!((shown(&id), shown(&other)), ("build-box".to_owned(), "build-box-2".to_owned()), "{list:?}");
    // A grant on it by the name people see is on the runner kiln knows by its id.
    let (st, body) = http(s.port, "POST", "/api/grants", &s.admin, Some(&json!({ "user": "dan", "grant": "runners.read@runner:build-box" })))?;
    anyhow::ensure!((200..300).contains(&st), "granting by its name: {st} {body}");
    let (_, body) = http(s.port, "GET", "/api/users", &s.admin, None)?;
    assert!(body.contains(&format!("runners.read@runner:{id}")) && !body.contains("runners.read@runner:build-box"), "{body}");

    // Forgotten, it stays out (W27-R01): its certificate for the id was recorded as it moved, so
    // forgetting it revokes its key, its certificate for its old name's with it.
    let key = kiln::pki::fingerprints(&std::fs::read_to_string(state.join("agent.pem"))?)?.key;
    let (st, body) = http(s.port, "DELETE", &format!("/api/agents/{id}"), &s.admin, None)?;
    anyhow::ensure!((200..300).contains(&st), "forgetting it: {st} {body}");
    let (_, body) = http(s.port, "GET", "/api/identities/revocations", &s.admin, None)?;
    let (_, issued) = http(s.port, "GET", "/api/identities?kind=machine", &s.admin, None)?;
    assert!(body.contains(&key), "its key is revoked: {body}\nwhat kiln issued: {issued}");
    std::thread::sleep(Duration::from_secs(6));
    until(s.port, &s.admin, "it stayed forgotten", &log, |l| !l.iter().any(|a| text(a, "name") == id))?;
    Ok(())
}

#[test]
fn a_runner_an_upgrade_started_moves_its_machine_to_its_machine_id_as_it_starts() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();
    let s = start(dir, &Options::default())?;
    let join = std::fs::read_to_string(dir.join("data/join.token"))?.trim().to_owned();
    let state = dir.join("machine");
    enrolled_by_name(s.port, &s.admin, &join, &state, "up-box")?;
    // Its usual runner connects by its name (it had nothing to move), then stops.
    let (first, log) = agent(dir, s.port, &state, "up-box", "first.log", &[])?;
    until(s.port, &s.admin, "up-box connected by its name", &log, |l| l.iter().any(|a| text(a, "name") == "up-box" && connected(a)))?;
    stop(first)?;
    until(s.port, &s.admin, "up-box gone away", &log, |l| l.iter().any(|a| text(a, "name") == "up-box" && !connected(a)))?;
    // Its runner now is the runner process an upgrade started (`--instance`): as it starts, the
    // machine moves to its id, and the process gets its own certificate for it.
    let (_beside, log) = agent(dir, s.port, &state, "up-box", "beside.log", &["--instance", "u1"])?;
    let id = machine("up-box");
    let list = until(s.port, &s.admin, "the runner process connected by the machine's id", &log, |l| l.iter().any(|a| text(a, "name") == id && connected(a)))?;
    let me = list.iter().find(|a| text(a, "name") == id).cloned().unwrap_or_default();
    assert_eq!(text(&me, "label"), "up-box", "shown by its name: {me}");
    assert!(!list.iter().any(|a| text(a, "name") == "up-box"), "nothing is known by its name any more: {list:?}");
    let said = std::fs::read_to_string(&log)?;
    assert!(said.contains("this runner is known by its machine id now") && said.contains("certificate for this runner process issued"), "{said}");
    Ok(())
}

#[test]
fn the_servers_own_runner_is_known_by_its_machines_id_and_shown_by_its_name() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();
    let s = start(dir, &Options { agent: true, ..Options::default() })?;
    // The test gives the server its machine (`KILN_MACHINE_ID`, `common::server_command`).
    let id = machine(&format!("server {}", dir.display()));
    let list = until(s.port, &s.admin, "the server's runner connected", Path::new(""), |l| l.iter().any(|a| text(a, "name") == id && connected(a)))?;
    let me = list.iter().find(|a| text(a, "name") == id).cloned().unwrap_or_default();
    assert_eq!(text(&me, "label"), kiln::pki::sanitize_name(&kiln::config::Config::hostname()), "shown by its configured name: {me}");
    assert_eq!(list.len(), 1, "one runner: {list:?}");
    Ok(())
}
