//! W27-R01 (docs/ci.md): revocations are recorded and checked. A runner forgotten has its
//! certificate revoked, with when, why and by whom; after the server restarts, the old
//! certificate is still refused; the machine enrolling again under the same name gets a new key
//! that works, and the old certificate stays refused.
//!
//! `cargo test -p kiln --test identities`

#![cfg(unix)]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, Server, http, start};
use serde_json::Value;

const TALK: Talk = Talk { lines: 1, every_ms: 10 };

/// How many times the runner's log says the server took it: a QUIC connection alone ("connected
/// to server") comes before the TLS directory decides.
fn connections(log: &Path) -> usize {
    std::fs::read_to_string(log).unwrap_or_default().matches("server welcomed agent").count()
}

/// Whether the server takes the runner again (its log says so once more) within `within`.
fn connects_again(log: &Path, within: Duration) -> bool {
    let (seen, deadline) = (connections(log), Instant::now() + within);
    while Instant::now() < deadline {
        if connections(log) > seen {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// The revocations of forgotten runners' machine certificates (runner processes' own
/// certificates are revoked as they're superseded, besides).
fn revocations(port: u16, admin: &str) -> anyhow::Result<Vec<Value>> {
    let (st, body) = http(port, "GET", "/api/identities/revocations", admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let all = serde_json::from_str::<Value>(&body)?.get("revocations").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(all.into_iter().filter(|r| r.get("reason").and_then(Value::as_str) == Some("runner forgotten")).collect())
}

#[test]
fn w27_r01_revocation_survives_restart_and_reenrolling_keeps_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut s = start(dir.path(), &Options::default())?;
    let state = dir.path().join("fedora-2-state");

    // Enrolled and connected; then forgotten.
    let (runner, log) = agents::runner(dir.path(), s.port, "fedora-2", None, TALK)?;
    agents::connected(s.port, &s.admin, "fedora-2", &log, Duration::from_secs(60))?;
    // Its own log has said so too (it may trail the server's list), so what follows counts anew.
    let deadline = Instant::now() + Duration::from_secs(30);
    while connections(&log) == 0 {
        anyhow::ensure!(Instant::now() < deadline, "the runner never logged its welcome");
        std::thread::sleep(Duration::from_millis(100));
    }
    let old_cert = std::fs::read_to_string(state.join("agent.pem"))?;
    let (st, body) = http(s.port, "DELETE", "/api/agents/fedora-2", &s.admin, None)?;
    anyhow::ensure!(st == 204, "forget: {st} {body}");
    let revs = revocations(s.port, &s.admin)?;
    anyhow::ensure!(revs.len() == 1, "one certificate revoked: {revs:?}");
    let rev = revs.first().cloned().unwrap_or_default();
    assert_eq!(rev.get("reason").and_then(Value::as_str), Some("runner forgotten"));
    assert_eq!(rev.get("by").and_then(Value::as_str), Some("admin"));
    assert!(rev.get("at").and_then(Value::as_i64).is_some_and(|at| at > 1_700_000_000), "{rev}");
    assert!(!connects_again(&log, Duration::from_secs(5)), "refused at once:\n{}", std::fs::read_to_string(&log)?);
    drop(runner);

    // The server restarts: the old certificate is still refused (before W27 a restart forgot it).
    s.restart()?;
    let (runner, log) = agents::runner(dir.path(), s.port, "fedora-2", None, TALK)?;
    assert!(!connects_again(&log, Duration::from_secs(8)), "the revoked certificate came back after a restart:\n{}\n{}", std::fs::read_to_string(&log)?, s.log());
    drop(runner);
    assert_eq!(revocations(s.port, &s.admin)?.len(), 1, "the revocation is kept");

    // Enrolled again under the same name: a new key, which works.
    let old_state = dir.path().join("fedora-2-old-state");
    std::fs::rename(&state, &old_state)?;
    let (_runner, log) = agents::runner(dir.path(), s.port, "fedora-2", None, TALK)?;
    agents::connected(s.port, &s.admin, "fedora-2", &log, Duration::from_secs(60))?;
    assert_ne!(std::fs::read_to_string(state.join("agent.pem"))?, old_cert, "enrolling again brings a new certificate");

    // The old certificate is still refused: a copy of the old runner, beside the new one.
    let copy = dir.path().join("fedora-2-copy-state");
    std::fs::rename(&old_state, &copy)?;
    let (_copy, copy_log) = agents::runner(dir.path(), s.port, "fedora-2-copy", None, TALK)?;
    assert!(!connects_again(&copy_log, Duration::from_secs(8)), "the old certificate connected after the name enrolled again:\n{}", std::fs::read_to_string(&copy_log)?);
    assert_eq!(revocations(s.port, &s.admin)?.len(), 1, "re-enrolling undid nothing");
    Ok(())
}

/// The identities kiln issued of `kind`, each with who it verifies as now (or why not).
fn identities(s: &Server, kind: &str) -> anyhow::Result<Vec<Value>> {
    let (st, body) = http(s.port, "GET", &format!("/api/identities?kind={kind}"), &s.admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    Ok(serde_json::from_str::<Value>(&body)?.get("identities").and_then(Value::as_array).cloned().unwrap_or_default())
}

/// Waits until `f` holds of the identities of `kind`.
fn until_identities(s: &Server, kind: &str, within: Duration, f: impl Fn(&[Value]) -> bool) -> anyhow::Result<Vec<Value>> {
    let deadline = Instant::now() + within;
    loop {
        let list = identities(s, kind)?;
        if f(&list) {
            return Ok(list);
        }
        anyhow::ensure!(Instant::now() < deadline, "identities of {kind}: {list:?}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn field<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// A second runner process of machine `name`, beside its first (a blue/green upgrade's), on the
/// same state.
fn instance(dir: &Path, port: u16, name: &str, id: &str) -> anyhow::Result<(agents::Runner, std::path::PathBuf)> {
    let join = std::fs::read_to_string(dir.join("data/join.token"))?.trim().to_owned();
    let log_path = dir.join(format!("{name}-{id}.log"));
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let child = common::kiln()
        .args(["agent", "--server", &format!("http://127.0.0.1:{port}"), "--token", &join, "--name", name, "--instance", id])
        .arg("--work-dir")
        .arg(dir.join(format!("{name}-{id}-work")))
        .arg("--state-dir")
        .arg(dir.join(format!("{name}-state")))
        .env("HOME", dir.join("home"))
        .env("NO_COLOR", "1")
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log))
        .spawn()?;
    Ok((agents::Runner(child), log_path))
}

/// W27-R01, R02: each runner process has a key of its own, made where it runs, and a certificate
/// naming its machine, its instance, who enrolled the machine and the organization; one retired
/// (gone while another of its machine goes on) is revoked from then.
#[test]
fn w27_r01_each_runner_process_its_own_identity() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (main, log) = agents::runner(dir.path(), s.port, "fedora-2", None, TALK)?;
    agents::connected(s.port, &s.admin, "fedora-2", &log, Duration::from_secs(60))?;
    let main_uri = "kiln://hyper-light/person/admin/machine/fedora-2/instance/main";
    let first = until_identities(&s, "runner", Duration::from_secs(30), |l| l.iter().any(|i| field(i, "subject") == main_uri))?;
    let mine = first.iter().find(|i| field(i, "subject") == main_uri).cloned().unwrap_or_default();
    assert_eq!(field(&mine, "verified"), "runner fedora-2 (instance main), enrolled by admin, of hyper-light", "{mine}");
    // Its certificate is on the machine; its key isn't anywhere on disk.
    let ids = dir.path().join("fedora-2-state/identities");
    let files: Vec<String> = std::fs::read_dir(&ids)?.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert!(files.iter().any(|f| f.starts_with("main-runner")), "{files:?}");
    for f in walk(dir.path())? {
        let text = std::fs::read_to_string(&f).unwrap_or_default();
        assert!(!(text.contains("PRIVATE KEY") && f.starts_with(&ids)), "a key written beside the identities: {}", f.display());
    }

    // A second process beside it: a key and certificate of its own.
    let (_b1, b1_log) = instance(dir.path(), s.port, "fedora-2", "b1")?;
    let b1_uri = "kiln://hyper-light/person/admin/machine/fedora-2/instance/b1";
    let both = until_identities(&s, "runner", Duration::from_secs(60), |l| l.iter().any(|i| field(i, "subject") == b1_uri && !field(i, "verified").is_empty()))
        .map_err(|e| anyhow::anyhow!("{e}\n{}", std::fs::read_to_string(&b1_log).unwrap_or_default()))?;
    let b1 = both.iter().find(|i| field(i, "subject") == b1_uri).cloned().unwrap_or_default();
    assert_eq!(field(&b1, "verified"), "runner fedora-2 (instance b1), enrolled by admin, of hyper-light");
    assert_ne!(field(&b1, "key"), field(&mine, "key"), "two processes, two keys");

    // The first goes while the second goes on: retired, its certificate revoked from then.
    drop(main);
    let after = until_identities(&s, "runner", Duration::from_secs(60), |l| l.iter().filter(|i| field(i, "subject") == main_uri).all(|i| field(i, "refused").contains("revoked")))?;
    let gone = after.iter().find(|i| field(i, "subject") == main_uri).cloned().unwrap_or_default();
    assert!(field(&gone, "refused").contains("runner process retired"), "{gone}");
    let still = after.iter().find(|i| field(i, "subject") == b1_uri).cloned().unwrap_or_default();
    assert!(!field(&still, "verified").is_empty(), "the second goes on: {still}");
    Ok(())
}

/// Every file under `dir`.
fn walk(dir: &Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d)?.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
            } else {
                out.push(p);
            }
        }
    }
    Ok(out)
}
