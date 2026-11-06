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
    agent_on(dir, port, state, name, name, log, more)
}

/// [`agent`], on machine `machine` (its machine id made from it): two names of one machine.
fn agent_on(dir: &Path, port: u16, state: &Path, name: &str, machine: &str, log: &str, more: &[&str]) -> anyhow::Result<(Runner, PathBuf)> {
    let home = dir.join("home");
    std::fs::create_dir_all(&home)?;
    let log_path = dir.join(log);
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let child = common::kiln()
        .args(["agent", "--server", &format!("http://127.0.0.1:{port}"), "--name", name, "--no-terminal"])
        .args(more)
        .env("KILN_MACHINE_ID", machine)
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

/// The machine whose state is `state` has a certificate naming it `name`, as a kiln from before
/// machine ids signed one (it registers no machine without its id now, docs/machines.md): signed
/// with the server's CA, in its data directory `dir/data`, as that server would have.
fn enrolled_by_name(dir: &Path, port: u16, admin: &str, state: &Path, name: &str) -> anyhow::Result<()> {
    let (csr, key) = kiln::pki::make_csr(name)?;
    let pki = kiln::pki::Pki::open(&dir.join("data/pki"))?;
    let cert = pki.sign_csr(&csr, name, 365)?;
    let (st, body) = http(port, "GET", "/api/agents/join", admin, None)?;
    anyhow::ensure!(st == 200, "the join info: {st} {body}");
    let info: Value = serde_json::from_str(&body)?;
    std::fs::create_dir_all(state)?;
    for (file, content) in [("agent.pem", cert.as_str()), ("agent.key", key.as_str()), ("ca.pem", pki.ca_pem.as_str()), ("quic", text(&info, "quic"))] {
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

/// docs/machines.md, "One record a machine": a runner whose certificate carries the name a kiln
/// from before gave it is keyed by its machine id the moment it connects, without a restart, and
/// shown by that name; started again, its certificate is for the id (a refresh: nothing moves).
#[test]
fn machines_a_runner_from_before_is_keyed_by_its_machine_id_as_it_connects() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();
    let s = start(dir, &Options::default())?;
    let state = dir.join("machine");
    enrolled_by_name(dir, s.port, &s.admin, &state, "old-box")?;
    let id = machine("old-box");

    // It connects with its certificate from before: known by its id at once, shown by its name.
    let (first, log) = agent(dir, s.port, &state, "old-box", "first.log", &[])?;
    let list = until(s.port, &s.admin, "the runner connected, keyed by its machine id", &log, |l| l.iter().any(|a| text(a, "name") == id && connected(a)))?;
    let me = list.iter().find(|a| text(a, "name") == id).cloned().unwrap_or_default();
    assert_eq!(text(&me, "label"), "old-box", "shown by its name: {me}");
    assert!(!list.iter().any(|a| text(a, "name") == "old-box"), "nothing is known by its name: {list:?}");
    // What's set for it, and a person's grant by the name people see, are the id's.
    let (st, body) = http(s.port, "PUT", &format!("/api/runners/{id}/agent-limit"), &s.admin, Some(&json!({ "max_sessions": 2 })))?;
    anyhow::ensure!(st == 200, "its agent limit: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": "dan", "grants": ["developer", "runners.terminal@runner:old-box"] })))?;
    anyhow::ensure!((200..300).contains(&st), "dan: {st} {body}");
    let (_, body) = http(s.port, "GET", "/api/users", &s.admin, None)?;
    assert!(body.contains(&format!("runners.terminal@runner:{id}")) && !body.contains("runners.terminal@runner:old-box"), "dan's grant is on the id: {body}");
    stop(first)?;
    until(s.port, &s.admin, "it went away", &log, |l| l.iter().any(|a| text(a, "name") == id && !connected(a)))?;

    // Started again: its certificate is for its id now, and it's the same record.
    let (_runner, log) = agent(dir, s.port, &state, "old-box", "runner.log", &[])?;
    until(s.port, &s.admin, "the runner connected by its machine id", &log, |l| l.iter().any(|a| text(a, "name") == id && connected(a)))?;
    assert!(std::fs::read_to_string(&log)?.contains("this runner is known by its machine id now"), "it took its certificate for the id");
    let leaf = std::fs::read_to_string(state.join("agent.pem"))?;
    assert_eq!(kiln::pki::leaf_name(&kiln::pki::Leaf { cert_pem: leaf.clone(), key_pem: String::new() }).as_deref(), Some(id.as_str()), "its certificate names its id");
    let (st, body) = http(s.port, "GET", &format!("/api/runners/{id}/agent-limit"), &s.admin, None)?;
    let limit: Value = serde_json::from_str(&body).unwrap_or_default();
    assert!(st == 200 && limit.get("max_sessions") == Some(&json!(2)), "its agent limit stayed: {st} {body}");

    // Renamed, it's shown by its new name, and that name is its machine's: another machine that
    // asks for it is shown by another.
    let (st, body) = http(s.port, "POST", &format!("/api/agents/{id}/label"), &s.admin, Some(&json!({ "label": "build-box" })))?;
    anyhow::ensure!(st == 200, "renaming it: {st} {body}");
    let (_other, other_log) = agents::runner(dir, s.port, "build-box", None, Talk { lines: 1, every_ms: 10 })?;
    let other = machine("build-box");
    let list = until(s.port, &s.admin, "the other machine connected", &other_log, |l| l.iter().any(|a| text(a, "name") == other && connected(a)))?;
    let shown = |k: &str| list.iter().find(|a| text(a, "name") == k).map(|a| text(a, "label").to_owned()).unwrap_or_default();
    assert_eq!((shown(&id), shown(&other)), ("build-box".to_owned(), "build-box-2".to_owned()), "{list:?}");

    // Forgotten, it stays out (W27-R01): its certificates' key is revoked, the one for its old name too.
    let key = kiln::pki::fingerprints(&leaf)?.key;
    let (st, body) = http(s.port, "DELETE", "/api/agents/old-box", &s.admin, None)?;
    anyhow::ensure!((200..300).contains(&st), "forgetting it by its old name: {st} {body}");
    let (_, body) = http(s.port, "GET", "/api/identities/revocations", &s.admin, None)?;
    assert!(body.contains(&key), "its key is revoked: {body}");
    std::thread::sleep(Duration::from_secs(6));
    until(s.port, &s.admin, "it stayed forgotten", &log, |l| !l.iter().any(|a| text(a, "name") == id))?;
    Ok(())
}

#[test]
fn a_runner_an_upgrade_started_moves_its_machine_to_its_machine_id_as_it_starts() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();
    let s = start(dir, &Options::default())?;
    let state = dir.join("machine");
    enrolled_by_name(dir, s.port, &s.admin, &state, "up-box")?;
    let id = machine("up-box");
    // Its usual runner connects (keyed by its machine id at once), then stops.
    let (first, log) = agent(dir, s.port, &state, "up-box", "first.log", &[])?;
    until(s.port, &s.admin, "up-box connected", &log, |l| l.iter().any(|a| text(a, "name") == id && connected(a)))?;
    stop(first)?;
    until(s.port, &s.admin, "up-box gone away", &log, |l| l.iter().any(|a| text(a, "name") == id && !connected(a)))?;
    // Its runner now is the runner process an upgrade started (`--instance`): as it starts, the
    // machine's certificate is for its id, and the process gets its own certificate for it.
    let (_beside, log) = agent(dir, s.port, &state, "up-box", "beside.log", &["--instance", "u1"])?;
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

/// docs/machines.md, "One record a machine": one machine whose runner had certificates for two
/// names from before (it joined by two host names, as fedora.attlocal.net and
/// kiln-a094a12d.attlocal.net did) is one record, by its id, whichever connects: the second
/// connecting is the same record, shown by the first's name, and both names resolve to it.
#[test]
fn machines_two_names_of_one_machine_are_one_record_by_its_id() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();
    let s = start(dir, &Options::default())?;
    let id = machine("one-box");
    for (name, log) in [("fedora", "fedora.log"), ("kiln-a094", "kiln-a094.log")] {
        let state = dir.join(name);
        enrolled_by_name(dir, s.port, &s.admin, &state, name)?;
        let (r, log) = agent_on(dir, s.port, &state, name, "one-box", log, &[])?;
        let list = until(s.port, &s.admin, &format!("{name} connected as the machine"), &log, |l| l.iter().any(|a| text(a, "name") == id && connected(a)))?;
        assert!(!list.iter().any(|a| ["fedora", "kiln-a094"].contains(&text(a, "name"))), "one record: {list:?}");
        assert_eq!(list.iter().filter(|a| text(a, "label") == "fedora").count(), 1, "shown by the first's name: {list:?}");
        stop(r)?;
        until(s.port, &s.admin, &format!("{name} gone away"), &log, |l| l.iter().any(|a| text(a, "name") == id && !connected(a)))?;
    }
    // Forgotten by either name, the machine is.
    let (st, body) = http(s.port, "DELETE", "/api/agents/kiln-a094", &s.admin, None)?;
    anyhow::ensure!((200..300).contains(&st), "forgetting it: {st} {body}");
    let (_, body) = http(s.port, "GET", "/api/agents", &s.admin, None)?;
    assert!(!body.contains(&id), "forgotten: {body}");
    Ok(())
}

/// docs/machines.md, "One record a machine": kiln registers machines by their ids. Enrolling with
/// the join token, and asking to join, without one are refused, saying how to get one.
#[test]
fn machines_registration_without_a_machine_id_is_refused() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();
    let s = start(dir, &Options::default())?;
    let join = std::fs::read_to_string(dir.join("data/join.token"))?.trim().to_owned();
    let (csr, _key) = kiln::pki::make_csr("no-id")?;
    let (st, body) = http(s.port, "POST", "/api/agents/enroll", &s.admin, Some(&json!({ "token": join, "name": "no-id", "csr": csr, "hostname": "no-id" })))?;
    assert!(st == 400 && body.contains("KILN_MACHINE_ID") && body.contains("systemd-machine-id-setup"), "enrolling without an id: {st} {body}");
    let id = machine("with-id");
    let (st, body) = http(s.port, "POST", "/api/agents/enroll", &s.admin, Some(&json!({ "token": join, "name": "with-id", "csr": csr, "hostname": "with-id", "machine_id": id })))?;
    assert!(st == 200 && body.contains(&id), "with its id it's registered by it: {st} {body}");
    Ok(())
}
