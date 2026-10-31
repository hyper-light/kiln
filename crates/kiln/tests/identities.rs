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

/// `fedora-2` enrolled and connected, then forgotten (`kiln forget fedora-2`'s route): its
/// certificate (PEM) and the revocation recorded for it, checked for when, why and by whom.
fn enrolled_then_forgotten(dir: &Path, s: &Server) -> anyhow::Result<(String, Value)> {
    let (runner, log) = agents::runner(dir, s.port, "fedora-2", None, TALK)?;
    agents::connected(s.port, &s.admin, "fedora-2", &log, Duration::from_secs(60))?;
    // Its own log has said so too (it may trail the server's list), so what follows counts anew.
    let deadline = Instant::now() + Duration::from_secs(30);
    while connections(&log) == 0 {
        anyhow::ensure!(Instant::now() < deadline, "the runner never logged its welcome");
        std::thread::sleep(Duration::from_millis(100));
    }
    let old_cert = std::fs::read_to_string(dir.join("fedora-2-state").join("agent.pem"))?;
    let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let (st, body) = http(s.port, "DELETE", "/api/agents/fedora-2", &s.admin, None)?;
    anyhow::ensure!(st == 204, "forget: {st} {body}");
    let revs = revocations(s.port, &s.admin)?;
    anyhow::ensure!(revs.len() == 1, "one certificate revoked: {revs:?}");
    let rev = revs.first().cloned().unwrap_or_default();
    assert_eq!(rev.get("serial").and_then(Value::as_str), Some(kiln::pki::fingerprints(&old_cert)?.serial.as_str()), "the certificate it had: {rev}");
    assert_eq!(rev.get("reason").and_then(Value::as_str), Some("runner forgotten"));
    assert_eq!(rev.get("by").and_then(Value::as_str), Some("admin"));
    assert!(rev.get("at").and_then(Value::as_i64).and_then(|at| u64::try_from(at).ok()).is_some_and(|at| at >= before && at <= before + 30), "revoked at that moment: {rev}");
    assert!(!connects_again(&log, Duration::from_secs(5)), "refused at once:\n{}", std::fs::read_to_string(&log)?);
    drop(runner);
    Ok((old_cert, rev))
}

/// What the chain checker says now of the certificate with `serial` (`GET /api/identities`).
fn checked(s: &Server, serial: &str) -> anyhow::Result<Value> {
    identities(s, "machine")?.into_iter().find(|i| field(i, "serial") == serial).ok_or_else(|| anyhow::anyhow!("kiln recorded no certificate {serial}"))
}

#[test]
fn w27_r01_revocation_survives_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut s = start(dir.path(), &Options::default())?;
    let (old_cert, rev) = enrolled_then_forgotten(dir.path(), &s)?;
    let serial = kiln::pki::fingerprints(&old_cert)?.serial;
    let at = rev.get("at").and_then(Value::as_i64).unwrap_or_default();
    let when = time::OffsetDateTime::from_unix_timestamp(at)?.format(&time::format_description::well_known::Rfc3339)?;
    let refusal = format!("revoked at {when} (runner forgotten, by admin)");

    // The server's Pki and store are opened again: the old certificate is still refused, at TLS
    // and by the chain checker, each naming the revocation's time and reason.
    s.restart()?;
    let (_runner, log) = agents::runner(dir.path(), s.port, "fedora-2", None, TALK)?;
    assert!(!connects_again(&log, Duration::from_secs(8)), "the revoked certificate came back after a restart:\n{}\n{}", std::fs::read_to_string(&log)?, s.log());
    let server_log = std::fs::read_to_string(dir.path().join("server.log"))?;
    assert!(
        server_log.lines().any(|l| l.contains("refused a revoked certificate") && l.contains(&refusal)),
        "the TLS directory's refusal names when and why ({refusal}):\n{}",
        s.log()
    );
    let c = checked(&s, &serial)?;
    assert!(field(&c, "refused").contains(&format!("was revoked at {when} (runner forgotten, by admin)")), "the chain checker's refusal: {c}");
    assert_eq!(revocations(s.port, &s.admin)?.len(), 1, "the revocation is kept");
    Ok(())
}

#[test]
fn w27_r01_reenroll_keeps_revocation() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (old_cert, _) = enrolled_then_forgotten(dir.path(), &s)?;
    let state = dir.path().join("fedora-2-state");

    // Enrolled again under the same name: a new key, which works.
    let old_state = dir.path().join("fedora-2-old-state");
    std::fs::rename(&state, &old_state)?;
    let (_runner, log) = agents::runner(dir.path(), s.port, "fedora-2", None, TALK)?;
    agents::connected(s.port, &s.admin, "fedora-2", &log, Duration::from_secs(60))?;
    let new_cert = std::fs::read_to_string(state.join("agent.pem"))?;
    assert_ne!(new_cert, old_cert, "enrolling again brings a new certificate");
    let (old, new) = (kiln::pki::fingerprints(&old_cert)?, kiln::pki::fingerprints(&new_cert)?);
    assert_ne!(old.key, new.key, "for a new key");
    assert!(field(&checked(&s, &new.serial)?, "refused").is_empty(), "the new certificate isn't refused");

    // The old certificate is still refused: a copy of the old runner, beside the new one.
    let copy = dir.path().join("fedora-2-copy-state");
    std::fs::rename(&old_state, &copy)?;
    let (_copy, copy_log) = agents::runner(dir.path(), s.port, "fedora-2-copy", None, TALK)?;
    assert!(!connects_again(&copy_log, Duration::from_secs(8)), "the old certificate connected after the name enrolled again:\n{}", std::fs::read_to_string(&copy_log)?);
    assert!(field(&checked(&s, &old.serial)?, "refused").contains("runner forgotten"), "the chain checker still refuses the old one");
    // The new one is still connected.
    agents::connected(s.port, &s.admin, "fedora-2", &log, Duration::from_secs(5))?;
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
    instance_with(dir, port, name, id, None)
}

/// [`instance`], with `chaos` its faults (a `kiln agent --chaos` spec; a build with the `chaos` feature).
fn instance_with(dir: &Path, port: u16, name: &str, id: &str, chaos: Option<&str>) -> anyhow::Result<(agents::Runner, std::path::PathBuf)> {
    let join = std::fs::read_to_string(dir.join("data/join.token"))?.trim().to_owned();
    let log_path = dir.join(format!("{name}-{id}.log"));
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
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
        .env("NO_COLOR", "1")
        // The stand-in harness on its PATH, as its first process has it: a newer process takes an
        // agent session over only with the session's harness there.
        .env("PATH", format!("{}:{}", dir.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
        .env("FAKE_CLAUDE_LINES", TALK.lines.to_string())
        .env("FAKE_CLAUDE_EVERY_MS", TALK.every_ms.to_string())
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
    // The runner writes its certificate once it has it, a moment after the server records it.
    let ids = dir.path().join("fedora-2-state/identities");
    let names = || -> Vec<String> { std::fs::read_dir(&ids).map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default() };
    let deadline = Instant::now() + Duration::from_secs(30);
    while !names().iter().any(|f| f.starts_with("main-runner")) {
        anyhow::ensure!(Instant::now() < deadline, "the runner never wrote its certificate: {:?}", names());
        std::thread::sleep(Duration::from_millis(100));
    }
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
/// Whether `bytes` hold a PEM private key: its BEGIN line, a base64 body and the END line that
/// matches it, as the leg's scan (SCAN) takes one. The words alone aren't one: the UI server kiln
/// carries names the header in a placeholder, a pattern and Bun's TLS strings.
fn pem_key(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    let mut rest: &str = &text;
    while let Some((_, after)) = rest.split_once("-----BEGIN ") {
        rest = after;
        let Some((label, body)) = after.split_once("-----") else { return false };
        if !label.ends_with("PRIVATE KEY") || !label.bytes().all(|b| b.is_ascii_uppercase() || b == b' ') {
            continue;
        }
        let Some((b64, _)) = body.split_once(&format!("-----END {label}-----")) else { continue };
        let base64 = b64.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'\n' | b'\r'));
        if base64 && b64.bytes().filter(|b| !b.is_ascii_whitespace()).count() >= 32 {
            return true;
        }
    }
    false
}

/// W27-AC02: the walk takes a PEM private key for one, never the words alone, which the UI server
/// kiln carries holds (a placeholder, a pattern, Bun's TLS strings) wherever it's built packed.
#[test]
fn w27_ac02_a_header_alone_is_no_key() -> anyhow::Result<()> {
    assert!(pem_key(kiln::pki::new_key_pem()?.as_bytes()), "a key is one");
    for not in [
        "placeholder=\"-----BEGIN PRIVATE KEY-----\"",
        "/-----BEGIN [A-Z ]*PRIVATE KEY-----/",
        "-----BEGIN PRIVATE KEY-----\u{0}\u{1}not base64\u{2}-----END PRIVATE KEY-----",
        "-----BEGIN PRIVATE KEY-----\n-----END PRIVATE KEY-----",
        "-----BEGIN CERTIFICATE-----\nMIIBkTCB+wIJAKHBfpegQ0SAMA0GCSqGSIb3DQEBCwUA\n-----END CERTIFICATE-----",
    ] {
        assert!(!pem_key(not.as_bytes()), "{not:?}");
    }
    Ok(())
}

fn walk(dir: &Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        // A folder gone since it was listed (a runner's temporary files) is skipped.
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.filter_map(|e| e.ok()) {
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

/// W27-R01, R02: an agent session has a key per epoch, made by the runner process holding it and
/// kept there; each epoch's certificate names the agent, the person it acts for, the epoch and the
/// runner process holding it; a hand-over to a newer process raises the epoch, and what the old
/// epoch signs from then is refused, naming both epochs. Started, then moved twice: exactly three
/// epochs certified. No session key is written anywhere, in the runners' state or the server's.
#[test]
fn w27_r01_agent_epochs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (_main, log) = agents::runner(dir.path(), s.port, "box-a", None, TALK)?;
    agents::connected(s.port, &s.admin, "box-a", &log, Duration::from_secs(60))?;
    let cwd = dir.path().join("work-here");
    std::fs::create_dir_all(&cwd)?;
    let id = agents::open_session(s.port, &s.admin, "box-a", &cwd, Duration::from_secs(60))?;
    // An epoch's subject: `…/agent/<id>/epoch/<n>`, then the runner process holding it.
    let epoch_of = |subject: &str| subject.split_once(&format!("/agent/{id}/epoch/")).and_then(|(_, rest)| rest.split('/').next()).and_then(|e| e.parse::<u64>().ok());
    let agent_of = |l: &[Value], epoch: u64| l.iter().find(|i| epoch_of(field(i, "subject")) == Some(epoch)).cloned();
    let first = until_identities(&s, "agent", Duration::from_secs(60), |l| l.iter().any(|i| epoch_of(field(i, "subject")).is_some() && !field(i, "verified").is_empty()))?;
    let e0 = first.iter().filter_map(|i| epoch_of(field(i, "subject"))).min().ok_or_else(|| anyhow::anyhow!("no epoch: {first:?}"))?;
    let v0 = agent_of(&first, e0).unwrap_or_default();
    assert_eq!(field(&v0, "verified"), format!("agent {id}, epoch {e0}, for admin, on runner box-a (instance main), of hyper-light"), "{v0}");

    // Two newer processes of the machine, one after the other: the session is handed to each, an
    // epoch up each time; the epoch it left is refused from the hand-over on, saying both epochs.
    let mut keep = Vec::new();
    for (n, inst) in ["z1", "z2"].iter().enumerate() {
        keep.push(instance(dir.path(), s.port, "box-a", inst)?);
        let want = e0 + 1 + u64::try_from(n)?;
        let list = until_identities(&s, "agent", Duration::from_secs(120), |l| {
            agent_of(l, want).is_some_and(|i| !field(&i, "verified").is_empty()) && agent_of(l, want - 1).is_some_and(|i| field(&i, "refused").contains("revoked"))
        })?;
        let now = agent_of(&list, want).unwrap_or_default();
        assert_eq!(field(&now, "verified"), format!("agent {id}, epoch {want}, for admin, on runner box-a (instance {inst}), of hyper-light"));
        let left = agent_of(&list, want - 1).unwrap_or_default();
        let said = format!("epoch {} left: moved on to epoch {want}, on runner box-a (instance {inst})", want - 1);
        assert!(field(&left, "refused").contains(&said), "{left}");
        assert_ne!(field(&now, "key"), field(&left, "key"), "a new epoch, a new key");
    }
    // Started, then moved twice: three epochs certified, no more, each naming the process that held it.
    let all = identities(&s, "agent")?;
    let mut held: Vec<(u64, String)> = all
        .iter()
        .filter_map(|i| epoch_of(field(i, "subject")).map(|e| (e, field(i, "subject").rsplit('/').next().unwrap_or_default().to_owned())))
        .collect();
    held.sort();
    held.dedup();
    assert_eq!(held, vec![(e0, "main".to_owned()), (e0 + 1, "z1".to_owned()), (e0 + 2, "z2".to_owned())], "{all:?}");

    // No session key on disk: not beside the certificates, not in the server's data or logs.
    for f in walk(dir.path())? {
        let name = f.to_string_lossy().into_owned();
        let watched = name.contains("/identities/") || name.ends_with("kiln.db") || name.ends_with(".log");
        if watched {
            let bytes = std::fs::read(&f).unwrap_or_default();
            assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE KEY"), "a private key in {name}");
        }
    }
    Ok(())
}

/// W27-R02: an agent session's key never leaves the runner process that made it. Every frame each
/// runner process sends is copied (a test build's `copy=`), and each key it makes (`keys=`); the
/// session is started, then moved twice. Its three keys, as PEM, as the PEM's body and as DER, are
/// in no frame, in nothing of the server's (its store, its logs) and nowhere in the runners' state.
///
/// `cargo test -p kiln --features chaos --test identities w27_r02_agent_key_stays`
#[cfg(feature = "chaos")]
#[test]
fn w27_r02_agent_key_stays() -> anyhow::Result<()> {
    use base64::Engine as _;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let spy = dir.path().join("spy");
    std::fs::create_dir_all(&spy)?;
    let chaos = |who: &str| format!("copy={},keys={}", spy.join(format!("{who}.frames")).display(), spy.join(format!("{who}.keys")).display());
    let (_main, log) = agents::runner(dir.path(), s.port, "box-a", Some(&chaos("main")), TALK)?;
    agents::connected(s.port, &s.admin, "box-a", &log, Duration::from_secs(60))?;
    let cwd = dir.path().join("work-here");
    std::fs::create_dir_all(&cwd)?;
    let id = agents::open_session(s.port, &s.admin, "box-a", &cwd, Duration::from_secs(60))?;
    let epoch_of = |subject: &str| subject.split_once(&format!("/agent/{id}/epoch/")).and_then(|(_, rest)| rest.split('/').next()).and_then(|e| e.parse::<u64>().ok());
    let verified_at = |l: &[Value], epoch: u64| l.iter().any(|i| epoch_of(field(i, "subject")) == Some(epoch) && !field(i, "verified").is_empty());
    let first = until_identities(&s, "agent", Duration::from_secs(60), |l| l.iter().any(|i| epoch_of(field(i, "subject")).is_some() && !field(i, "verified").is_empty()))?;
    let e0 = first.iter().filter_map(|i| epoch_of(field(i, "subject"))).min().ok_or_else(|| anyhow::anyhow!("no epoch: {first:?}"))?;
    let mut keep = Vec::new();
    for (n, inst) in ["z1", "z2"].iter().enumerate() {
        keep.push(instance_with(dir.path(), s.port, "box-a", inst, Some(&chaos(inst)))?);
        let want = e0 + 1 + u64::try_from(n)?;
        until_identities(&s, "agent", Duration::from_secs(120), |l| verified_at(l, want))?;
    }

    // The session's keys, as each process copied them out: one an epoch, made where it was held.
    let mut keys = Vec::new();
    for who in ["main", "z1", "z2"] {
        let text = std::fs::read_to_string(spy.join(format!("{who}.keys"))).unwrap_or_default();
        let mut lines = text.lines();
        while let Some(slot) = lines.next() {
            if !slot.starts_with(&format!("agent/{id}/")) {
                continue;
            }
            let mut pem = String::new();
            for l in lines.by_ref() {
                pem.push_str(l);
                pem.push('\n');
                if l.starts_with("-----END") {
                    break;
                }
            }
            keys.push(pem);
        }
    }
    assert_eq!(keys.len(), 3, "a key an epoch, three epochs: {}", keys.len());
    // Each as PEM text, as the PEM's body on one line, and as the DER it encodes.
    let mut needles: Vec<Vec<u8>> = Vec::new();
    for pem in &keys {
        let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
        needles.push(base64::engine::general_purpose::STANDARD.decode(&body)?);
        needles.push(body.into_bytes());
        needles.push(pem.as_bytes().to_vec());
    }

    // What crossed the network, every frame each process sent; then everything on disk but the copies.
    let mut frames = Vec::new();
    for who in ["main", "z1", "z2"] {
        frames.extend(std::fs::read(spy.join(format!("{who}.frames")))?);
    }
    assert!(frames.len() > 1024, "the runners' frames were copied: {} bytes", frames.len());
    let mut places: Vec<(String, Vec<u8>)> = vec![("the frames the runners sent".to_owned(), frames)];
    for f in walk(dir.path())? {
        if f.starts_with(&spy) {
            continue;
        }
        places.push((f.to_string_lossy().into_owned(), std::fs::read(&f).unwrap_or_default()));
    }
    let holds = |hay: &[u8], needle: &[u8]| !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle);
    let found: Vec<&str> = places.iter().filter(|(_, bytes)| needles.iter().any(|n| holds(bytes, n))).map(|(name, _)| name.as_str()).collect();
    assert!(found.is_empty(), "a session's key outside the runner process that made it: {found:?}");
    drop(keep);
    Ok(())
}

/// One request with a session cookie (`None`: none): status, body and head.
fn call(port: u16, method: &str, path: &str, cookie: Option<&str>, body: Option<&Value>) -> anyhow::Result<(u16, String, String)> {
    use std::io::{Read as _, Write as _};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let payload = body.map(Value::to_string).unwrap_or_default();
    let cookie = cookie.map(|c| format!("Cookie: kiln_session={c}\r\n")).unwrap_or_default();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{cookie}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len())?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status: u16 = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { common::dechunk(rest) } else { rest.to_owned() };
    Ok((status, body, head.to_owned()))
}

/// Person `name` added by the owner, as a developer.
fn add_person(s: &Server, name: &str) -> anyhow::Result<()> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&serde_json::json!({ "name": name, "grants": ["developer"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    Ok(())
}

/// The one-time sign-in link the owner makes for `name`, to hand them.
fn sign_in_link(s: &Server, name: &str) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", &format!("/api/users/{name}/link"), &s.admin, None)?;
    anyhow::ensure!(st == 200, "{name}'s link: {st} {body}");
    serde_json::from_str::<Value>(&body)?.get("url").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no link: {body}"))
}

/// `name` signed in in a browser with the sign-in link the owner made: their session's cookie.
fn browser_session(s: &Server, name: &str) -> anyhow::Result<String> {
    let url = sign_in_link(s, name)?;
    let path = url.find("/api/").and_then(|at| url.get(at..)).ok_or_else(|| anyhow::anyhow!("a link without a path: {url}"))?;
    let (_, _, head) = call(s.port, "GET", path, None, None)?;
    head.lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .find_map(|l| l.split_once(':').map(|(_, v)| v.trim()).and_then(|v| v.split(';').next()).and_then(|v| v.strip_prefix("kiln_session=")))
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("{name} wasn't signed in: {head}"))
}

/// A person's own device: its home and config, where `kiln` keeps their key.
struct Device {
    home: std::path::PathBuf,
    config: std::path::PathBuf,
}

impl Device {
    fn new(dir: &Path, name: &str) -> anyhow::Result<Device> {
        let d = Device { home: dir.join(name).join("home"), config: dir.join(name).join("config") };
        std::fs::create_dir_all(&d.home)?;
        std::fs::create_dir_all(&d.config)?;
        Ok(d)
    }

    /// `kiln args` run here (`stdin` its input): whether it succeeded, and what it said. No token
    /// of this machine's reaches it (git's credential helper is given one that isn't anyone's).
    fn kiln(&self, args: &[&str], stdin: Option<&str>) -> anyhow::Result<(bool, String)> {
        use std::io::Write as _;
        let mut child = common::kiln()
            .args(args)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("KILN_TOKEN", "nobody")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut input) = child.stdin.take() {
            input.write_all(stdin.unwrap_or_default().as_bytes())?;
        }
        let out = child.wait_with_output()?;
        Ok((out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))))
    }

    /// git asking this device's credential helper about the server on `port`.
    fn credential_helper(&self, port: u16) -> anyhow::Result<String> {
        Ok(self.kiln(&["git-credential", "get"], Some(&format!("protocol=http\nhost=127.0.0.1:{port}\n\n")))?.1)
    }

    fn file(&self, name: &str) -> std::path::PathBuf {
        self.config.join("kiln").join(name)
    }
}

/// kiln's root: its CA's certificate, as the server publishes it.
fn root(s: &Server) -> anyhow::Result<Vec<u8>> {
    let (st, pem) = http(s.port, "GET", "/ca.pem", "", None)?;
    anyhow::ensure!(st == 200, "{st} {pem}");
    kiln::pki::der_of(&pem)
}

/// Every revocation kiln recorded, for the chain checker.
fn revocations_now(s: &Server) -> anyhow::Result<Vec<kiln_ci::identity::chain::Revocation>> {
    let (st, body) = http(s.port, "GET", "/api/identities/revocations", &s.admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let rows = serde_json::from_str::<Value>(&body)?.get("revocations").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(rows
        .iter()
        .map(|r| kiln_ci::identity::chain::Revocation {
            serial: r.get("serial").and_then(Value::as_str).map(str::to_owned),
            key: r.get("key").and_then(Value::as_str).map(str::to_owned),
            at: r.get("at").and_then(Value::as_i64).unwrap_or_default(),
            reason: field(r, "reason").to_owned(),
            by: field(r, "by").to_owned(),
        })
        .collect())
}

fn unix_now() -> anyhow::Result<i64> {
    Ok(i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs())?)
}

/// That no file under `paths` holds the private key `key_pem`: its DER or a line of its PEM.
fn nowhere(paths: &[std::path::PathBuf], key_pem: &str) -> anyhow::Result<()> {
    use base64::Engine as _;
    let body: String = key_pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let der = base64::engine::general_purpose::STANDARD.decode(&body)?;
    let lines: Vec<&str> = key_pem.lines().filter(|l| !l.starts_with("-----") && l.len() >= 32).collect();
    anyhow::ensure!(!der.is_empty() && !lines.is_empty(), "not a key: {key_pem}");
    for p in paths {
        let files = if p.is_dir() { walk(p)? } else { vec![p.clone()] };
        for f in files {
            let bytes = std::fs::read(&f).unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes);
            assert!(!bytes.windows(der.len()).any(|w| w == der.as_slice()), "the key's DER in {}", f.display());
            assert!(!lines.iter().any(|l| text.contains(l)), "the key's PEM in {}", f.display());
        }
    }
    Ok(())
}

/// W27-R03, AC03: an agent's commit is signed as the agent, for its person, never with the
/// person's key, and its push goes as the agent: git in the agent's session (the fake harness
/// acting on a message, as an agent's tool would) commits with the git the runner set up for it;
/// the runner holding the session's key signs through `kiln-git-sign`, and the commit verifies to
/// kiln's root as the agent at its epoch, on its runner. Its push to a repository kiln hosts takes
/// the agent's own credential (the runner asks for it as that session), so kiln records it as the
/// agent's, for its person; and the person's credential helper for kiln is never asked.
#[test]
fn w27_ac03_an_agents_commit_verifies_as_the_agent() -> anyhow::Result<()> {
    use kiln_ci::identity::chain::{Actor, Identity};
    use kiln_ci::identity::gitsig;
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let root = root(&s)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&serde_json::json!({"name": "acme/agents", "mode": "native"})))?;
    anyhow::ensure!(st == 200, "a repository kiln hosts: {st} {body}");
    let cwd = dir.path().join("work-here");
    std::fs::create_dir_all(&cwd)?;
    // The person's own credential for kiln, in the agent's folder: a helper that says it was asked.
    let theirs = dir.path().join("persons-helper.sh");
    std::fs::write(&theirs, format!("#!/bin/sh\necho asked >> '{}'\necho username=person\necho password=the-persons\n", dir.path().join("persons-helper.out").display()))?;
    std::fs::set_permissions(&theirs, std::fs::Permissions::from_mode(0o755))?;
    // What the agent does with the message: a commit in its folder and a push of it to kiln, its
    // git as the runner set it.
    let act = dir.path().join("act.sh");
    let push = format!("http://127.0.0.1:{}/git/acme/agents.git", s.port);
    std::fs::write(
        &act,
        format!(
            "#!/bin/sh\ngit init -q -b main 2>/dev/null\ngit config --local credential.helper '!{}'\ngit -c user.name=agent -c user.email=agent@kiln commit -q --allow-empty -m \"the agent's\" >> act.out 2>&1\necho \"commit $?\" >> act.out\nGIT_TERMINAL_PROMPT=0 git push -q {push} HEAD:refs/heads/agent/work >> act.out 2>&1\necho \"push $?\" >> act.out\necho \"exit\" >> act.out\n",
            theirs.display()
        ),
    )?;
    std::fs::set_permissions(&act, std::fs::Permissions::from_mode(0o755))?;
    let act_path = act.to_string_lossy().into_owned();
    let (_main, log) = agents::runner_with(dir.path(), s.port, "box-a", None, TALK, &[("FAKE_CLAUDE_ACT", &act_path)])?;
    agents::connected(s.port, &s.admin, "box-a", &log, Duration::from_secs(60))?;
    let id = agents::open_session(s.port, &s.admin, "box-a", &cwd, Duration::from_secs(60))?;
    let epoch = format!("/agent/{id}/epoch/");
    until_identities(&s, "agent", Duration::from_secs(60), |l| l.iter().any(|i| field(i, "subject").contains(&epoch) && !field(i, "verified").is_empty()))?;
    let (st, said) = http(s.port, "POST", &format!("/api/harness/sessions/{id}/command"), &s.admin, Some(&serde_json::json!({"type": "send", "id": "commit", "text": "commit it"})))?;
    anyhow::ensure!((200..300).contains(&st), "send: {st} {said}");
    let done = || std::fs::read_to_string(cwd.join("act.out")).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done().contains("exit") {
        anyhow::ensure!(Instant::now() < deadline, "the agent never committed: {}", std::fs::read_to_string(&log).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(200));
    }
    anyhow::ensure!(done().contains("commit 0"), "the agent's commit failed: {}", done());
    anyhow::ensure!(done().contains("push 0"), "the agent's push failed: {}", done());
    let out = std::process::Command::new("git").arg("-C").arg(&cwd).args(["cat-file", "commit", "HEAD"]).output()?;
    let sig = gitsig::signature_of(&out.stdout).ok_or_else(|| anyhow::anyhow!("the agent's commit isn't signed: {}", String::from_utf8_lossy(&out.stdout)))?;
    let v = gitsig::verify(&gitsig::payload(&out.stdout), &sig, &root, &revocations_now(&s)?, unix_now()?)?;
    assert!(matches!(&v.identity.actor, Actor::Agent { agent, held_by: Some(h), .. } if *agent == id && h.machine == "box-a"), "as the agent, on its runner: {v}");
    assert!(!v.identity.is_person("admin"), "never as its person: {v}");
    assert_eq!(v.acts_for(), vec![Identity::new("hyper-light", Actor::Person { name: "admin".into() })], "for its person");
    // Its push went as the agent, for its person; the person's own credential was never asked for.
    let (_, audit) = http(s.port, "GET", "/api/audit?limit=200", &s.admin, None)?;
    let audit: serde_json::Value = serde_json::from_str(&audit)?;
    let pushes: Vec<&serde_json::Value> = audit.as_array().into_iter().flatten().filter(|e| field(e, "action") == "repo.push").collect();
    assert!(pushes.iter().any(|e| field(e, "actor") == format!("agent:{id} for admin")), "pushed as the agent, for its person: {pushes:?}");
    assert!(pushes.iter().all(|e| field(e, "principal").starts_with("agent:")), "never as its person: {pushes:?}");
    assert!(!dir.path().join("persons-helper.out").exists(), "the person's credential helper was asked");
    Ok(())
}

/// W27-R03, AC03: a person's commit is signed with their key, on their device. sam signs in
/// (`kiln login`); git, set up as `kiln clone` sets it up, signs a commit through `kiln-git-sign`
/// with that device's key; the commit's signature verifies to kiln's root as sam, never as anyone
/// else, and the key never left the laptop.
#[test]
fn w27_ac03_a_persons_commit_verifies_as_them() -> anyhow::Result<()> {
    use kiln_ci::identity::chain::{Actor, Identity};
    use kiln_ci::identity::gitsig;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let root = root(&s)?;
    add_person(&s, "sam")?;
    let link = sign_in_link(&s, "sam")?;
    let laptop = Device::new(dir.path(), "sam-laptop")?;
    let (ok, said) = laptop.kiln(&["login", "--link", &link], None)?;
    anyhow::ensure!(ok, "kiln login --link: {said}");
    // git's signing program: kiln's own binary by its name, as `kiln clone` places it.
    let bin = dir.path().join("sam-laptop").join("bin");
    std::fs::create_dir_all(&bin)?;
    let program = bin.join(kiln::gitsign::NAME);
    if std::fs::hard_link(env!("CARGO_BIN_EXE_kiln"), &program).is_err() {
        std::fs::copy(env!("CARGO_BIN_EXE_kiln"), &program)?;
    }
    let repo = dir.path().join("sam-laptop").join("app");
    std::fs::create_dir_all(&repo)?;
    let git = |args: &[&str]| -> anyhow::Result<std::process::Output> {
        Ok(std::process::Command::new("git").arg("-C").arg(&repo).args(args).env("HOME", &laptop.home).env("XDG_CONFIG_HOME", &laptop.config).env("GIT_CONFIG_NOSYSTEM", "1").output()?)
    };
    git(&["init", "-q", "-b", "main"])?;
    let mut config = vec![("user.name", "sam".to_owned()), ("user.email", "sam@example.com".to_owned())];
    config.extend(kiln::gitsign::git_config(&program, kiln::gitsign::DEVICE_KEY));
    for (k, v) in &config {
        git(&["config", "--local", k, v.as_str()])?;
    }
    let out = git(&["commit", "-q", "--allow-empty", "-m", "sam's first"])?;
    anyhow::ensure!(out.status.success(), "git commit: {}", String::from_utf8_lossy(&out.stderr));
    let raw = git(&["cat-file", "commit", "HEAD"])?.stdout;
    let sig = gitsig::signature_of(&raw).ok_or_else(|| anyhow::anyhow!("sam's commit isn't signed: {}", String::from_utf8_lossy(&raw)))?;
    let v = gitsig::verify(&gitsig::payload(&raw), &sig, &root, &revocations_now(&s)?, unix_now()?)?;
    assert_eq!(v.identity, Identity::new("hyper-light", Actor::Person { name: "sam".into() }), "{v}");
    // Signed on the laptop: the server saw neither the commit nor the key.
    let key = std::fs::read_to_string(laptop.file("identity.key"))?;
    nowhere(&[dir.path().join("data"), dir.path().join("server.log")], &key)?;
    Ok(())
}

/// W27-R03, AC03: a repository kiln hosts takes only what verifies, as its policy says. One from
/// before (made while the server's new repositories only warned) takes an unsigned push and says
/// which commit isn't signed, before and after the server comes to require signatures of new ones.
/// A new one refuses that push, naming the commit and why; takes sam's signed commit (its policy
/// with it); refuses sam's commit where that policy takes only kim's; and once sam is disabled,
/// their certificate revoked, refuses what sam signs, saying so. Each refusal is in git's own
/// report, and no refused ref moves.
#[test]
fn w27_ac03_an_unsigned_push_is_refused() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // New repositories warn at first (the tests' default): `acme/old` is one from before.
    let mut s = start(dir.path(), &Options::default())?;
    let make = |s: &Server, name: &str| -> anyhow::Result<()> {
        let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&serde_json::json!({"name": name, "mode": "native"})))?;
        anyhow::ensure!(st == 200, "{name}: {st} {body}");
        Ok(())
    };
    make(&s, "acme/old")?;
    // sam signs on their laptop, as `kiln clone` sets git up there.
    add_person(&s, "sam")?;
    let link = sign_in_link(&s, "sam")?;
    let laptop = Device::new(dir.path(), "sam-laptop")?;
    let (ok, said) = laptop.kiln(&["login", "--link", &link], None)?;
    anyhow::ensure!(ok, "kiln login --link: {said}");
    let bin = dir.path().join("sam-laptop").join("bin");
    std::fs::create_dir_all(&bin)?;
    let program = bin.join(kiln::gitsign::NAME);
    if std::fs::hard_link(env!("CARGO_BIN_EXE_kiln"), &program).is_err() {
        std::fs::copy(env!("CARGO_BIN_EXE_kiln"), &program)?;
    }
    let work = dir.path().join("sam-laptop").join("work");
    std::fs::create_dir_all(work.join(".kiln"))?;
    let git = |args: &[&str]| -> anyhow::Result<std::process::Output> {
        Ok(std::process::Command::new("git").arg("-C").arg(&work).args(args).env("HOME", &laptop.home).env("XDG_CONFIG_HOME", &laptop.config).env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_TERMINAL_PROMPT", "0").output()?)
    };
    git(&["init", "-q", "-b", "main"])?;
    let mut config = vec![("user.name", "sam".to_owned()), ("user.email", "sam@example.com".to_owned())];
    config.extend(kiln::gitsign::git_config(&program, kiln::gitsign::DEVICE_KEY));
    for (k, v) in &config {
        git(&["config", "--local", k, v.as_str()])?;
    }
    let commit = |message: &str, signed: bool| -> anyhow::Result<(String, String)> {
        let out = git(&["commit", "-q", "--allow-empty", if signed { "--gpg-sign" } else { "--no-gpg-sign" }, "-m", message])?;
        anyhow::ensure!(out.status.success(), "git commit: {}", String::from_utf8_lossy(&out.stderr));
        let sha = String::from_utf8_lossy(&git(&["rev-parse", "HEAD"])?.stdout).trim().to_owned();
        let short = sha.get(..7).unwrap_or_default().to_owned();
        Ok((sha, short))
    };
    let push = |s: &Server, repo: &str, spec: &str| -> anyhow::Result<(bool, String)> {
        let out = git(&["push", &format!("http://x:{}@127.0.0.1:{}/git/{repo}.git", s.admin, s.port), spec])?;
        Ok((out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))))
    };
    let at = |s: &Server, repo: &str, branch: &str| -> anyhow::Result<String> {
        let out = git(&["ls-remote", &format!("http://x:{}@127.0.0.1:{}/git/{repo}.git", s.admin, s.port), &format!("refs/heads/{branch}")])?;
        Ok(String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap_or_default().to_owned())
    };
    let (_, unsigned) = commit("unsigned", false)?;
    let (ok, said) = push(&s, "acme/old", "HEAD:main")?;
    anyhow::ensure!(ok, "a repository that warns takes it: {said}");
    assert!(said.contains(&format!("warning: {unsigned} isn't signed (.kiln/review.yaml can require signatures)")), "{said}");

    // Then new repositories require signatures, as kiln's default has them.
    let toml = std::fs::read_to_string(dir.path().join("kiln.toml"))?;
    std::fs::write(dir.path().join("kiln.toml"), toml.replace("signatures = \"warn\"", "signatures = \"required\""))?;
    s.restart()?;
    make(&s, "acme/new")?;
    let (ok, said) = push(&s, "acme/new", "HEAD:main")?;
    assert!(!ok && said.contains("[remote rejected]") && said.contains(&format!("(commit {unsigned} isn't signed: main takes commits signed by people, agents or kiln of hyper-light)")), "{said}");
    assert_eq!(at(&s, "acme/new", "main")?, "", "nothing moved");
    // The one from before still only warns.
    let (_, again) = commit("unsigned again", false)?;
    let (ok, said) = push(&s, "acme/old", "HEAD:main")?;
    anyhow::ensure!(ok, "{said}");
    assert!(said.contains(&format!("warning: {again} isn't signed")), "{said}");

    // sam's signed commit lands, with a policy that takes only kim's signatures on release branches.
    git(&["checkout", "-q", "--orphan", "signed"])?;
    std::fs::write(work.join(".kiln/review.yaml"), "signatures:\n  signers:\n    - refs: [\"release/*\"]\n      by: [kim]\n")?;
    git(&["add", ".kiln/review.yaml"])?;
    let (policy, _) = commit("policy", true)?;
    let (ok, said) = push(&s, "acme/new", "HEAD:main")?;
    anyhow::ensure!(ok, "sam's signed commit: {said}");
    assert_eq!(at(&s, "acme/new", "main")?, policy);
    let (_, sams) = commit("sam's release fix", true)?;
    let (ok, said) = push(&s, "acme/new", "HEAD:refs/heads/release/1")?;
    assert!(!ok && said.contains(&format!("(commit {sams} is signed by sam: release/1 takes commits signed by kim of hyper-light)")), "{said}");
    assert_eq!(at(&s, "acme/new", "release/1")?, "", "nothing moved");
    let (ok, said) = push(&s, "acme/new", "HEAD:refs/heads/topic")?;
    anyhow::ensure!(ok, "a branch no rule names takes sam's: {said}");

    // sam is disabled, which revokes their certificate: what they sign now is refused.
    let (st, body) = http(s.port, "DELETE", "/api/users/sam", &s.admin, None)?;
    anyhow::ensure!((200..300).contains(&st), "disable sam: {st} {body}");
    let (_, late) = commit("after sam was disabled", true)?;
    let (ok, said) = push(&s, "acme/new", "HEAD:refs/heads/topic")?;
    assert!(!ok && said.contains(&format!("(commit {late}'s signature doesn't verify (")) && said.contains("was revoked at") && said.contains("person disabled"), "{said}");
    Ok(())
}

/// W27-R03: a person who signs with SSH binds their key to their identity, and a commit it signs
/// verifies as them on a push: git signs through `ssh-keygen` (`gpg.format = ssh`), as people's git
/// does. A key bound to no one is refused where signatures are required, and so is a key once
/// unbound; an agent can't bind one for its person.
#[test]
fn w27_r03_a_persons_ssh_key_signs_as_them() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { ci: "signatures = \"required\"", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&serde_json::json!({"name": "acme/ssh", "mode": "native"})))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    add_person(&s, "sam")?;
    let token = |kind: &str| -> anyhow::Result<String> {
        let mut ask = serde_json::json!({ "kind": kind, "user": "sam", "grants": ["developer"], "expires_in_days": 1 });
        if kind == "agent"
            && let Some(o) = ask.as_object_mut()
        {
            o.insert("agent".into(), serde_json::json!("matrix"));
        }
        let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&ask))?;
        anyhow::ensure!(st == 201, "a {kind} token for sam: {st} {body}");
        Ok(field(&serde_json::from_str::<Value>(&body)?, "token").to_owned())
    };
    let sam = token("pat")?;
    // Two keys on sam's laptop, as ssh-keygen makes them: one bound, one not.
    let keys = dir.path().join("sam-keys");
    std::fs::create_dir_all(&keys)?;
    for name in ["bound", "loose"] {
        let made = std::process::Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-C", &format!("sam@{name}"), "-f"]).arg(keys.join(name)).status()?;
        anyhow::ensure!(made.success(), "ssh-keygen");
    }
    let public = |name: &str| -> anyhow::Result<String> { Ok(std::fs::read_to_string(keys.join(format!("{name}.pub")))?.trim().to_owned()) };
    let (st, body) = http(s.port, "POST", "/api/me/ssh-signers", &sam, Some(&serde_json::json!({ "key": public("bound")? })))?;
    anyhow::ensure!(st == 200, "bind sam's key: {st} {body}");
    let fingerprint = serde_json::from_str::<Value>(&body)?.pointer("/signer/fingerprint").and_then(Value::as_str).unwrap_or_default().to_owned();
    assert!(fingerprint.starts_with("SHA256:"), "{body}");
    let (st, body) = http(s.port, "GET", "/api/me/ssh-signers", &sam, None)?;
    assert!(st == 200 && body.contains(&fingerprint) && body.contains("sam@bound"), "{st} {body}");
    // An agent can't bind a key for its person.
    let (st, body) = http(s.port, "POST", "/api/me/ssh-signers", &token("agent")?, Some(&serde_json::json!({ "key": public("loose")? })))?;
    assert_eq!((st, field(&serde_json::from_str::<Value>(&body)?, "code")), (403, "not_a_person"), "{body}");

    // sam's git signs with SSH.
    let home = dir.path().join("sam-home");
    let work = dir.path().join("sam-work");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&work)?;
    let git = |args: &[&str]| -> anyhow::Result<std::process::Output> {
        Ok(std::process::Command::new("git").arg("-C").arg(&work).args(args).env("HOME", &home).env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_TERMINAL_PROMPT", "0").output()?)
    };
    git(&["init", "-q", "-b", "main"])?;
    for (k, v) in [("user.name", "sam"), ("user.email", "sam@example.com"), ("gpg.format", "ssh")] {
        git(&["config", "--local", k, v])?;
    }
    let signed_with = |key: &str, message: &str| -> anyhow::Result<String> {
        git(&["config", "--local", "user.signingkey", &keys.join(key).to_string_lossy()])?;
        let out = git(&["commit", "-q", "--allow-empty", "--gpg-sign", "-m", message])?;
        anyhow::ensure!(out.status.success(), "git commit: {}", String::from_utf8_lossy(&out.stderr));
        Ok(String::from_utf8_lossy(&git(&["rev-parse", "HEAD"])?.stdout).trim().to_owned())
    };
    let push = |spec: &str| -> anyhow::Result<(bool, String)> {
        let out = git(&["push", &format!("http://x:{}@127.0.0.1:{}/git/acme/ssh.git", s.admin, s.port), spec])?;
        Ok((out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))))
    };
    let sha = signed_with("bound", "sam's, signed with SSH")?;
    let (ok, said) = push("HEAD:main")?;
    anyhow::ensure!(ok, "sam's SSH-signed commit: {said}");
    let raw = String::from_utf8_lossy(&git(&["cat-file", "commit", "HEAD"])?.stdout).into_owned();
    assert!(raw.contains("-----BEGIN SSH SIGNATURE-----"), "git signed it with SSH: {raw}");
    let listed = String::from_utf8_lossy(&git(&["ls-remote", &format!("http://x:{}@127.0.0.1:{}/git/acme/ssh.git", s.admin, s.port), "refs/heads/main"])?.stdout).into_owned();
    assert!(listed.starts_with(&sha), "{listed}");
    // A key bound to no one: refused, saying so.
    let loose = signed_with("loose", "signed with a key kiln doesn't know")?;
    let (ok, said) = push("HEAD:refs/heads/topic")?;
    assert!(!ok && said.contains(&format!("(commit {}'s signature doesn't verify (its SSH key (SHA256:", loose.get(..7).unwrap_or_default())) && said.contains("is bound to no one in kiln"), "{said}");
    // Unbound, sam's first key signs for no one either.
    let query = fingerprint.replace(':', "%3A").replace('+', "%2B").replace('/', "%2F");
    let (st, body) = http(s.port, "DELETE", &format!("/api/me/ssh-signers?fingerprint={query}"), &sam, None)?;
    assert!(st == 200 && body.contains("\"removed\":true"), "{st} {body}");
    git(&["reset", "-q", "--hard", &sha])?;
    signed_with("bound", "after the key was unbound")?;
    let (ok, said) = push("HEAD:refs/heads/topic")?;
    assert!(!ok && said.contains("is bound to no one in kiln"), "{said}");
    Ok(())
}

/// W27-R01, R02: a person signs in from the CLI on their own device and gets a certificate for a
/// key made there. It verifies to kiln's root as the person, of their organization, for hours at
/// most; the key is in their config, theirs alone (0600), and nowhere on the server, which keeps
/// the certificate. They sign in with a sign-in link, a password or a personal access token; the
/// credential helper renews the certificate while they stay signed in, and not after.
#[test]
fn w27_r01_person_login() -> anyhow::Result<()> {
    use kiln_ci::identity::chain::{Actor, Identity, verify};
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let url = format!("http://127.0.0.1:{}", s.port);
    let root = root(&s)?;

    // sam signs in on their laptop with the sign-in link they were given.
    add_person(&s, "sam")?;
    let link = sign_in_link(&s, "sam")?;
    let laptop = Device::new(dir.path(), "sam-laptop")?;
    let (ok, said) = laptop.kiln(&["login", "--link", &link], None)?;
    anyhow::ensure!(ok, "kiln login --link: {said}");
    assert!(said.contains("Certified: sam of hyper-light until "), "{said}");

    // The key is the laptop's, its owner's alone; the certificate is for it.
    let key = std::fs::read_to_string(laptop.file("identity.key"))?;
    assert!(key.contains("PRIVATE KEY"));
    assert_eq!(std::fs::metadata(laptop.file("identity.key"))?.permissions().mode() & 0o777, 0o600, "0600");
    let cert = std::fs::read_to_string(laptop.file("identity.pem"))?;
    assert!(kiln::pki::cert_matches_key(&cert, &key), "the certificate is for the laptop's key");

    // It verifies to kiln's root as sam, of hyper-light, for hours.
    let v = verify(&[&kiln::pki::der_of(&cert)?], &root, &revocations_now(&s)?, unix_now()?)?;
    assert_eq!(v.identity, Identity::new("hyper-light", Actor::Person { name: "sam".into() }));
    assert_eq!(v.to_string(), "sam of hyper-light");
    assert!(v.identity.is_person("sam") && v.acts_for().is_empty(), "sam, acting for no one but themself, of hyper-light");
    let lived = v.not_after - v.not_before;
    assert!(lived > 3600 && lived <= 24 * 3600, "hours, never past a day: {lived} s");

    // The server keeps the certificate and never the key: not in its store (nor the store's
    // write-ahead log), its data or its log.
    let kept = identities(&s, "person")?;
    assert!(kept.iter().any(|i| field(i, "serial") == v.serial && field(i, "key") == v.key && field(i, "verified") == "sam of hyper-light"), "{kept:?}");
    for f in walk(&dir.path().join("data"))?.into_iter().filter(|f| f.file_name().is_some_and(|n| n.to_string_lossy().starts_with("kiln.db"))) {
        assert!(!String::from_utf8_lossy(&std::fs::read(&f)?).contains("PRIVATE KEY"), "a private key in the store: {}", f.display());
    }
    nowhere(&[dir.path().join("data"), dir.path().join("server.log")], &key)?;

    // Renewed while sam stays signed in: git's credential helper asks again once it's due (here,
    // gone), for the same key.
    std::fs::remove_file(laptop.file("identity.pem"))?;
    let said = laptop.credential_helper(s.port)?;
    let renewed = std::fs::read_to_string(laptop.file("identity.pem")).map_err(|e| anyhow::anyhow!("the credential helper didn't renew it ({e}): {said}"))?;
    let r = verify(&[&kiln::pki::der_of(&renewed)?], &root, &revocations_now(&s)?, unix_now()?)?;
    assert_eq!(r.to_string(), "sam of hyper-light");
    assert_ne!(r.serial, v.serial, "a new certificate");
    assert_eq!(r.key, v.key, "for the same key");
    // Not before it's due: nothing new.
    laptop.credential_helper(s.port)?;
    assert_eq!(std::fs::read_to_string(laptop.file("identity.pem"))?, renewed);

    // Signed out everywhere: no longer renewed. The helper says so once and forgets the sign-in.
    let (st, body) = http(s.port, "POST", "/api/users/sam/sign-out", &s.admin, None)?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    std::fs::remove_file(laptop.file("identity.pem"))?;
    let said = laptop.credential_helper(s.port)?;
    assert!(said.contains("no longer signed in"), "{said}");
    assert!(!laptop.file("identity.pem").exists() && !laptop.file("identity.json").exists(), "{said}");
    // A used link signs no one in.
    let (ok, said) = Device::new(dir.path(), "elsewhere")?.kiln(&["login", "--link", &link], None)?;
    assert!(!ok && said.contains("not signed in"), "{said}");

    // ada with her password, on her desktop.
    add_person(&s, "ada")?;
    let cookie = browser_session(&s, "ada")?;
    let password = "a long passphrase of ada's";
    let (st, body, _) = call(s.port, "PUT", "/api/me/password", Some(&cookie), Some(&serde_json::json!({ "password": password })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let desk = Device::new(dir.path(), "ada-desk")?;
    let (ok, said) = desk.kiln(&["login", "--url", &url, "--name", "ada", "--password-stdin"], Some(&format!("{password}\n")))?;
    anyhow::ensure!(ok, "kiln login --name: {said}");
    assert!(said.contains("Certified: ada of hyper-light"), "{said}");

    // The owner with a personal access token, on their phone: the owner's tokens name the owner.
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&serde_json::json!({ "kind": "pat", "grants": ["owner"], "expires_in_days": 1 })))?;
    anyhow::ensure!(st == 201, "{st} {body}");
    let pat = field(&serde_json::from_str::<Value>(&body)?, "token").to_owned();
    let phone = Device::new(dir.path(), "owner-phone")?;
    let (ok, said) = phone.kiln(&["login", "--url", &url, "--token", &pat], None)?;
    anyhow::ensure!(ok, "kiln login --token: {said}");
    assert!(said.contains("Certified: admin of hyper-light"), "{said}");
    let p = verify(&[&kiln::pki::der_of(&std::fs::read_to_string(phone.file("identity.pem"))?)?], &root, &revocations_now(&s)?, unix_now()?)?;
    assert!(p.identity.is_person("admin"), "{p}");
    for (device, key) in [(&desk, desk.file("identity.key")), (&phone, phone.file("identity.key"))] {
        assert_eq!(std::fs::metadata(&key)?.permissions().mode() & 0o777, 0o600, "{}", device.config.display());
        nowhere(&[dir.path().join("data"), dir.path().join("server.log")], &std::fs::read_to_string(&key)?)?;
    }

    // Only after a sign-in, and only a person's: nobody signed in gets none, nor does an agent.
    let kp = rcgen::KeyPair::generate()?;
    let csr = rcgen::CertificateParams::new(Vec::<String>::new())?.serialize_request(&kp)?.pem()?;
    let (st, _, _) = call(s.port, "POST", "/api/me/identity", None, Some(&serde_json::json!({ "csr": csr })))?;
    assert_eq!(st, 401);
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&serde_json::json!({ "kind": "agent", "agent": "matrix", "grants": ["owner"] })))?;
    anyhow::ensure!(st == 201, "{st} {body}");
    let agent = field(&serde_json::from_str::<Value>(&body)?, "token").to_owned();
    let (st, body) = http(s.port, "POST", "/api/me/identity", &agent, Some(&serde_json::json!({ "csr": csr })))?;
    assert_eq!(st, 403, "{body}");
    assert_eq!(field(&serde_json::from_str::<Value>(&body)?, "code"), "not_a_person", "{body}");
    Ok(())
}

/// W27-R01: disabling a person refuses their signatures from that moment, naming it: what sam's
/// laptop signed before still verifies as sam; what it signs from then is refused, saying when,
/// why and by whom; and sam is certified no more.
#[test]
fn w27_r01_person_disabled() -> anyhow::Result<()> {
    use kiln_ci::identity::chain::{Refusal, verify_signed};
    use rcgen::SigningKey as _;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let root = root(&s)?;
    add_person(&s, "sam")?;
    let laptop = Device::new(dir.path(), "sam-laptop")?;
    let (ok, said) = laptop.kiln(&["login", "--link", &sign_in_link(&s, "sam")?], None)?;
    anyhow::ensure!(ok, "kiln login --link: {said}");
    let cert = kiln::pki::der_of(&std::fs::read_to_string(laptop.file("identity.pem"))?)?;
    let key = rcgen::KeyPair::from_pem(&std::fs::read_to_string(laptop.file("identity.key"))?)?;

    // sam signs a statement on the laptop: it's sam's.
    let early = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n";
    let (early_at, early_sig) = (unix_now()?, key.sign(early)?);
    let v = verify_signed(&[&cert], &root, &revocations_now(&s)?, early_at, early, &early_sig)?;
    assert_eq!(v.to_string(), "sam of hyper-light");

    // A moment later the owner disables sam (10:02, in the plan's words).
    std::thread::sleep(Duration::from_millis(1100));
    let before = unix_now()?;
    let (st, body) = http(s.port, "DELETE", "/api/users/sam", &s.admin, None)?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let after = unix_now()?;
    let revs = revocations_now(&s)?;
    let disabled = revs.iter().find(|r| r.reason == "person disabled").cloned().ok_or_else(|| anyhow::anyhow!("no revocation: {revs:?}"))?;
    assert!(disabled.at >= before && disabled.at <= after, "revoked at the moment sam was disabled: {disabled:?}");
    assert_eq!(disabled.by, "admin");
    let when = time::OffsetDateTime::from_unix_timestamp(disabled.at)?.format(&time::format_description::well_known::Rfc3339)?;

    // What the laptop signs from that moment is refused, naming it; what it signed before still
    // verifies.
    let late = b"tree 9bfc3e3f0b6a17e8b4c5d6a7e8f9012345678901\n";
    let late_sig = key.sign(late)?;
    for at in [disabled.at, unix_now()?.max(disabled.at) + 60] {
        let got = verify_signed(&[&cert], &root, &revs, at, late, &late_sig);
        assert!(
            matches!(&got, Err(e @ Refusal::Revoked { revoked_at, reason, by, .. })
                if *revoked_at == when && reason == "person disabled" && by == "admin" && e.to_string().contains(&format!("was revoked at {when} (person disabled, by admin)"))),
            "a signature at {at}: {got:?}"
        );
    }
    assert!(early_at < disabled.at, "signed before");
    assert_eq!(verify_signed(&[&cert], &root, &revs, early_at, early, &early_sig)?.to_string(), "sam of hyper-light", "what sam signed before still verifies");
    let listed = identities(&s, "person")?;
    let sams = listed.iter().find(|i| field(i, "subject") == "kiln://hyper-light/person/sam").cloned().unwrap_or_default();
    assert!(field(&sams, "refused").contains(&format!("revoked at {when} (person disabled, by admin)")), "{sams}");

    // sam is certified no more: the laptop's sign-in ended with them.
    std::fs::remove_file(laptop.file("identity.pem"))?;
    let said = laptop.credential_helper(s.port)?;
    assert!(said.contains("no longer signed in"), "{said}");
    assert!(!laptop.file("identity.pem").exists(), "{said}");
    let (st, _) = http(s.port, "POST", "/api/users/sam/link", &s.admin, None)?;
    assert_eq!(st, 404, "no sign-in link for someone disabled");
    Ok(())
}

fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = std::process::Command::new("git")
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

/// The leg's steps look for a key in what they can reach (its environment), and find none. The
/// words are split so the workflow's own text (kept in the store) never holds them.
const LEG: &str = r#"on: push
jobs:
  e2e:
    runs-on: local
    steps:
      - run: |
          if env | grep -q "PRIVATE"" KEY"; then echo "a private key in the environment"; exit 1; fi
          sleep 1
          echo "no key here"
"#;

/// A leg's workflow for the test runner alone (no other runner is up), its step `run`.
fn leg_workflow(run: &str) -> String {
    format!("on: push\njobs:\n  e2e:\n    runs-on: self-hosted\n    steps:\n      - run: {run}\n")
}

/// A server whose one runner is `fedora-2` (enrolled with the join token, `env` in its
/// environment), and a repository of its own whose push of `workflow` (`.kiln/e2e.yaml`) runs:
/// returned once the run has ended, successful, with the commit it ran at.
struct LegRun {
    s: Server,
    runner: agents::Runner,
    sha: String,
}

fn leg_on_a_test_runner(dir: &Path, workflow: &str, env: &[(&str, &str)]) -> anyhow::Result<LegRun> {
    leg_on_a_test_runner_with(dir, workflow, env, None)
}

/// [`leg_on_a_test_runner`], the runner a test build with `chaos`'s faults (`old`: a runner from
/// before what it can do past `Build`'s 32 bits).
fn leg_on_a_test_runner_with(dir: &Path, workflow: &str, env: &[(&str, &str)], chaos: Option<&str>) -> anyhow::Result<LegRun> {
    let s = start(dir, &Options { agent: false, ci: "kiln_workflows = true", extra: "" })?;
    let (runner, log) = agents::runner_with(dir, s.port, "fedora-2", chaos, TALK, env)?;
    agents::connected(s.port, &s.admin, "fedora-2", &log, Duration::from_secs(60))?;
    let sha = push_e2e(dir, &s, workflow)?;
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=10", &s.admin, None)?;
        let runs: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        if let Some(r) = runs.iter().find(|r| r.get("finished_at").is_some_and(|f| !f.is_null())) {
            let said = || format!("{r}\n{}\n{}", s.log(), std::fs::read_to_string(&log).unwrap_or_default());
            anyhow::ensure!(r.get("conclusion").and_then(Value::as_str) == Some("success"), "the run failed: {}", said());
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "no finished run: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(LegRun { s, runner, sha })
}

/// [`leg_on_a_test_runner`] on v1: the server takes `.kiln/` workflows to v1, having no `[ci]
/// kiln_workflows` (as production's has none), and its job `e2e` runs on the test runner to its end.
fn v1_job_on_a_test_runner(dir: &Path, workflow: &str) -> anyhow::Result<LegRun> {
    let s = start(dir, &Options { agent: false, ci: "", extra: "" })?;
    let (runner, log) = agents::runner_with(dir, s.port, "fedora-2", None, TALK, &[])?;
    agents::connected(s.port, &s.admin, "fedora-2", &log, Duration::from_secs(60))?;
    // Its runner's build known first, so v1 writes the job for what the runner can do. A job given
    // to it before (its capabilities reading none) goes out of step: read with no steps, and
    // passed without running (W05-R22).
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = http(s.port, "GET", "/api/updates", &s.admin, None)?;
        let runners = serde_json::from_str::<Value>(&body)?.get("runners").and_then(Value::as_array).cloned().unwrap_or_default();
        if runners.iter().any(|r| field(r, "name") == "fedora-2" && r.get("build").is_some_and(|b| !b.is_null())) {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "fedora-2's build never known: {body}");
        std::thread::sleep(Duration::from_millis(200));
    }
    let sha = push_e2e(dir, &s, workflow)?;
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let db = rusqlite::Connection::open(dir.join("data/kiln.db"))?;
        let status: Option<String> = db.query_row("SELECT status FROM jobs WHERE key = 'e2e' ORDER BY rowid DESC LIMIT 1", [], |r| r.get(0)).ok().flatten();
        match status.as_deref() {
            Some("succeeded") => break,
            Some(end @ ("failed" | "cancelled" | "skipped" | "timed_out")) => anyhow::bail!("the job {end}: {}\n{}", s.log(), std::fs::read_to_string(&log).unwrap_or_default()),
            _ => {}
        }
        anyhow::ensure!(Instant::now() < deadline, "the job never ended: {status:?}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(LegRun { s, runner, sha })
}

/// Repository acme/app on `s`, pushed once polled, then `workflow` as `.kiln/e2e.yaml`: its commit.
fn push_e2e(dir: &Path, s: &Server, workflow: &str) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&serde_json::json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.join("githome"), dir.join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "an app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    // Once the server has polled its trunk, the workflow comes as a push.
    let deadline = Instant::now() + Duration::from_secs(60);
    while http(s.port, "GET", "/api/repos", &s.admin, None)?.1.contains("\"last_polled_at\":null") {
        anyhow::ensure!(Instant::now() < deadline, "never polled");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::write(work.join(".kiln/e2e.yaml"), workflow)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "the workflow"])?;
    let sha = git(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    Ok(sha)
}

/// The leg's certificate as kiln issued it (its store's record of it), and kiln's root: DER.
fn leg_certificate(dir: &Path) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let db = rusqlite::Connection::open(dir.join("data/kiln.db"))?;
    let pem: String = db.query_row("SELECT cert_pem FROM identities WHERE kind = 'leg' ORDER BY issued_at DESC LIMIT 1", [], |r| r.get(0))?;
    let root = std::fs::read_to_string(dir.join("data/pki/ca.pem"))?;
    Ok((kiln::pki::der_of(&pem)?, kiln::pki::der_of(&root)?))
}

/// The leg `e2e`'s timeout (minutes), as its spec in the store has it.
fn leg_timeout(dir: &Path) -> anyhow::Result<i64> {
    let db = rusqlite::Connection::open(dir.join("data/kiln.db"))?;
    let spec: String = db.query_row("SELECT leg FROM jobs WHERE key = 'e2e'", [], |r| r.get(0))?;
    serde_json::from_str::<Value>(&spec)?.get("timeout_minutes").and_then(Value::as_i64).ok_or_else(|| anyhow::anyhow!("no timeout in {spec}"))
}

/// Who owns a process's memory file: its own user while it's dumpable, root while it isn't.
#[cfg(target_os = "linux")]
fn memory_owner(pid: u32) -> anyhow::Result<u32> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(std::fs::metadata(format!("/proc/{pid}/mem"))?.uid())
}

/// W27-R01, R02: a CI leg run on a test runner has a certificate of its own, for a key that runner
/// made: it verifies to kiln's root as the leg of its run and attempt, at its commit, on its
/// runner instance, and through that runner who enrolled it and the organization; W10's claims
/// for it are in Fulcio's extensions (its workflow by ref and digest, its repository, commit and
/// ref, the event, the run), as kiln's Sigstore verifier reads them too; it lasts as long as the
/// attempt may, and once the leg is over its key is gone from the runner.
#[test]
fn w27_r01_leg_certificate() -> anyhow::Result<()> {
    use kiln_ci::identity::chain::{Actor, Identity, Kind, verify};
    let dir = tempfile::tempdir()?;
    let run = leg_on_a_test_runner(dir.path(), &leg_workflow("echo e2e"), &[])?;
    let (leaf, root) = leg_certificate(dir.path())?;
    let base = format!("http://127.0.0.1:{}", run.s.port);
    let now = i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs())?;
    let v = verify(&[&leaf], &root, &[], now)?;
    let short = run.sha.get(..7).unwrap_or_default();
    assert_eq!(v.to_string(), format!("leg e2e of run 1 (attempt 1) at {short}, on runner fedora-2 (instance main), enrolled by admin, of hyper-light"));
    assert_eq!(v.identity, Identity::new("hyper-light", Actor::Leg { enrolled_by: "admin".into(), machine: "fedora-2".into(), instance: "main".into(), run: 1, attempt: 1, leg: "e2e".into() }));
    let acts_for: Vec<String> = v.acts_for().iter().map(ToString::to_string).collect();
    assert_eq!(acts_for, ["runner fedora-2 (instance main), enrolled by admin, of hyper-light", "admin of hyper-light"]);
    assert_eq!(v.acts_for().iter().map(Identity::kind).collect::<Vec<_>>(), [Kind::Runner, Kind::Person]);
    let f = &v.facts;
    let workflow = format!("{base}/acme/app/.kiln/e2e.yaml@refs/heads/main");
    assert_eq!(f.workflow.as_deref(), Some(workflow.as_str()), "{f:?}");
    assert_eq!(f.workflow_digest.as_deref(), Some(run.sha.as_str()));
    assert_eq!(f.repository.as_deref(), Some(format!("{base}/acme/app").as_str()));
    assert_eq!(f.commit.as_deref(), Some(run.sha.as_str()));
    assert_eq!(f.git_ref.as_deref(), Some("refs/heads/main"));
    assert_eq!(f.trigger.as_deref(), Some("push"));
    assert_eq!(f.issuer.as_deref(), Some(format!("{base}/oidc").as_str()));
    let invoked = f.run.clone().unwrap_or_default();
    assert!(invoked.starts_with(&format!("{base}/acme/app/actions/runs/")) && invoked.ends_with("/attempts/1"), "{invoked}");
    // kiln's existing Sigstore verifier reads the same facts, by Fulcio's names.
    let (issuer, subject, claims) = kiln::ci::sigstore::certificate_identity(&leaf)?;
    assert_eq!((issuer.as_str(), subject.as_str()), (format!("{base}/oidc").as_str(), "kiln://hyper-light/person/admin/machine/fedora-2/instance/main/run/1/attempt/1/leg/e2e"));
    let claim = |k: &str| claims.get(k).cloned().unwrap_or_default();
    assert_eq!((claim("build_signer_uri"), claim("build_signer_digest")), (workflow, run.sha.clone()));
    assert_eq!((claim("source_repository_digest"), claim("run_invocation_uri")), (run.sha.clone(), invoked));
    // As long as its attempt may run (its timeout) and an hour more, from five minutes before it
    // was issued.
    let timeout = leg_timeout(dir.path())?;
    let (lived, want) = (v.not_after - v.not_before, (timeout + 60) * 60 + 300);
    assert!((want - 1..=want + 1).contains(&lived), "{lived} s for a timeout of {timeout} min");
    // The leg over, its runner let go of its key: its certificate's copy is gone, and the runner's
    // memory, kept from its user's other processes meanwhile, is theirs to read again.
    let ids = dir.path().join("fedora-2-state/identities");
    let names: Vec<String> = std::fs::read_dir(&ids)?.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert!(!names.iter().any(|n| n.starts_with("main-leg")), "{names:?}");
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt as _;
        let pid = run.runner.0.id();
        assert_eq!(memory_owner(pid)?, std::fs::metadata(dir.path())?.uid(), "the runner is dumpable again");
        std::fs::read(format!("/proc/{pid}/environ")).map_err(|e| anyhow::anyhow!("the runner's memory is still kept: {e}"))?;
    }
    Ok(())
}

/// What a leg's step lists of what it can reach (W27-R02): its environment, every file it can
/// read under `$W27_ROOTS` and `/dev/shm`, the environment and command line of every process it
/// sees, and the memory of the processes it runs under (its shell, its runner and the one above),
/// where it may read it. Every private key it finds goes to `$W27_FOUND`: PEM blocks, and P-256
/// keys as DER (PKCS #8, SEC 1) by their scalar. The words are split so its own text holds none.
const SCAN: &str = r#"
import json, os, re

found = {"env": 0, "files": 0, "keys": [], "memory": {}, "owners": {}}
PEM = re.compile(rb"-----BEGIN ((?:EC |RSA )?PRIV" rb"ATE KEY)-----[A-Za-z0-9+/=\r\n]+-----END \1-----")
DER = [bytes.fromhex("308187020100301306072a8648ce3d020106082a8648ce3d030107046d306b0201010420"), bytes.fromhex("30770201010420")]


def scan(where, data):
    for m in PEM.finditer(data):
        found["keys"].append({"where": where, "pem": m.group(0).decode("ascii", "replace")})
    for head in DER:
        at = data.find(head)
        while at != -1:
            scalar = data[at + len(head):at + len(head) + 32]
            if len(scalar) == 32:
                found["keys"].append({"where": where, "scalar": scalar.hex()})
            at = data.find(head, at + 1)


for k, v in os.environ.items():
    found["env"] += 1
    scan("env " + k, (k + "=" + v).encode())
for root in os.environ["W27_ROOTS"].split(os.pathsep) + ["/dev/shm"]:
    for d, _, names in os.walk(root):
        for name in names:
            path = os.path.join(d, name)
            try:
                with open(path, "rb") as f:
                    data = f.read(64 << 20)
            except OSError:
                continue
            found["files"] += 1
            scan(path, data)
pids = [p for p in os.listdir("/proc") if p.isdigit()] if os.path.isdir("/proc") else []
for pid in pids:
    for part in ("environ", "cmdline"):
        try:
            with open(f"/proc/{pid}/{part}", "rb") as f:
                scan(f"/proc/{pid}/{part}", f.read())
        except OSError:
            pass
pid = os.getppid()
for _ in range(3):
    if pid <= 1 or str(pid) not in pids:
        break
    try:
        found["owners"][str(pid)] = os.stat(f"/proc/{pid}/mem").st_uid
        with open(f"/proc/{pid}/maps") as f:
            maps = f.read().splitlines()
        mem = open(f"/proc/{pid}/mem", "rb")
    except OSError:
        found["memory"][str(pid)] = "refused"
    else:
        read = 0
        with mem:
            for line in maps:
                span, perms = line.split()[:2]
                lo, hi = (int(x, 16) for x in span.split("-"))
                if perms[0] != "r" or hi - lo > 1 << 30:
                    continue
                try:
                    mem.seek(lo)
                    data = mem.read(hi - lo)
                except (OSError, OverflowError, ValueError):
                    continue
                read += len(data)
                scan(f"/proc/{pid}/mem", data)
        found["memory"][str(pid)] = read
    try:
        with open(f"/proc/{pid}/stat") as f:
            pid = int(f.read().rsplit(")", 1)[1].split()[1])
    except (OSError, ValueError, IndexError):
        break
with open(os.environ["W27_FOUND"], "w") as f:
    json.dump(found, f)
print(f"{found['env']} variables, {found['files']} files, {len(pids)} processes, the memory of {len(found['memory'])}: {len(found['keys'])} private keys")
"#;

/// W27-R02: no step of a leg can read the leg's private key. The leg's step lists its environment
/// and every file it can read (its runner's state, workspace and logs, the server's data, its
/// home), the environment and command line of every process, and its runner's memory: of the
/// private keys it finds there (its runner's machine key among them: a step runs as its runner's
/// user), none is the leg's, which its runner made and holds in memory the step can't read.
#[test]
fn w27_r02_no_step_reads_the_legs_key() -> anyhow::Result<()> {
    use rcgen::PublicKeyData as _;
    let dir = tempfile::tempdir()?;
    let (scan, found) = (dir.path().join("scan.py"), dir.path().join("found.json"));
    std::fs::write(&scan, SCAN)?;
    let (scan, found_at, roots) = (scan.display().to_string(), found.display().to_string(), dir.path().display().to_string());
    let run = leg_on_a_test_runner(dir.path(), &leg_workflow("python3 \"$W27_SCAN\""), &[("W27_SCAN", &scan), ("W27_FOUND", &found_at), ("W27_ROOTS", &roots)])?;
    let seen: Value = serde_json::from_slice(&std::fs::read(&found)?)?;
    let (leaf, _) = leg_certificate(dir.path())?;
    let (_, cert) = x509_parser::parse_x509_certificate(&leaf)?;
    let leg_key = cert.public_key().subject_public_key.data.to_vec();
    // Each key found, by its public half.
    let public = |k: &Value| -> Option<Vec<u8>> {
        if let Some(pem) = k.get("pem").and_then(Value::as_str) {
            return rcgen::KeyPair::from_pem(pem).ok().map(|kp| kp.der_bytes().to_vec());
        }
        let scalar = hex::decode(k.get("scalar").and_then(Value::as_str)?).ok()?;
        let pkcs8 = [hex::decode("3041020100301306072a8648ce3d020106082a8648ce3d030107042730250201010420").ok()?, scalar].concat();
        rcgen::KeyPair::try_from(pkcs8.as_slice()).ok().map(|kp| kp.der_bytes().to_vec())
    };
    let keys = seen.get("keys").and_then(Value::as_array).cloned().unwrap_or_default();
    let occurrences: Vec<&str> = keys.iter().filter(|k| public(k).as_deref() == Some(leg_key.as_slice())).map(|k| field(k, "where")).collect();
    eprintln!(
        "the step looked at {} variables and {} files, and the memory of {}: {} private keys, {} of them the leg's",
        seen.get("env").unwrap_or(&Value::Null),
        seen.get("files").unwrap_or(&Value::Null),
        seen.get("memory").unwrap_or(&Value::Null),
        keys.len(),
        occurrences.len()
    );
    assert_eq!(occurrences.len(), 0, "the leg's private key, where its step could read it: {occurrences:?}");
    // It looked: at its environment, at files, and it read the runner's own (its machine's key).
    let count = |k: &str| seen.get(k).and_then(Value::as_u64).unwrap_or_default();
    assert!(count("env") > 0 && count("files") > 10, "{seen}");
    assert!(keys.iter().any(|k| field(k, "where").ends_with("fedora-2-state/agent.key") && public(k).is_some()), "the step reads its runner's files: {keys:?}");
    // And its runner's memory was refused it on Linux, whatever Yama's ptrace scope says: while the
    // leg ran its runner wasn't dumpable, its memory root's.
    if cfg!(target_os = "linux") {
        let runner = run.runner.0.id().to_string();
        let memory = seen.get("memory").cloned().unwrap_or_default();
        assert_eq!(memory.get(&runner).and_then(Value::as_str), Some("refused"), "{memory}");
        let owners = seen.get("owners").cloned().unwrap_or_default();
        assert_eq!(owners.get(&runner).and_then(Value::as_u64), Some(0), "{owners}");
    }
    Ok(())
}

/// W27-R01 (identities plan step 7): the server and CD sign as themselves. Their certificates verify
/// to kiln's root, as the server and as CD for the server; their keys are made in the server's data,
/// each in its own 0600 file and nowhere else there, and kept across a restart. One revoked, by its
/// certificate or by its key (a revocation takes the key with it), is replaced at once: a new key,
/// in its own file, and a certificate for it.
#[test]
fn w27_r01_server_and_cd() -> anyhow::Result<()> {
    use kiln_ci::identity::chain::{Actor, Identity, verify};
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir()?;
    let mut s = start(dir.path(), &Options::default())?;
    let server = until_identities(&s, "server", Duration::from_secs(10), |l| !l.is_empty())?;
    assert_eq!(field(server.first().unwrap_or(&Value::Null), "verified"), "kiln server of hyper-light");
    let cd = until_identities(&s, "cd", Duration::from_secs(10), |l| !l.is_empty())?;
    assert_eq!(field(cd.first().unwrap_or(&Value::Null), "verified"), "CD, for the kiln server of hyper-light");
    let (pki, data) = (dir.path().join("data/pki"), dir.path().join("data"));
    let read = |f: &str| std::fs::read_to_string(pki.join(f));
    let server_of = Identity::new("hyper-light", Actor::Server);
    // Each verifies to kiln's root: the server as itself, CD as acting for it.
    fn verified(s: &Server, pki: &Path, file: &str) -> anyhow::Result<kiln_ci::identity::chain::Verified> {
        Ok(verify(&[&kiln::pki::der_of(&std::fs::read_to_string(pki.join(file))?)?], &root(s)?, &revocations_now(s)?, unix_now()?)?)
    }
    fn revoke(s: &Server, what: &Value) -> anyhow::Result<()> {
        let (st, body) = http(s.port, "POST", "/api/identities/revoke", &s.admin, Some(what))?;
        anyhow::ensure!(st == 200, "{st} {body}");
        Ok(())
    }
    let v = verified(&s, &pki, "server-signing.pem")?;
    assert_eq!((v.identity.clone(), v.to_string()), (server_of.clone(), "kiln server of hyper-light".to_owned()));
    let v = verified(&s, &pki, "cd-signing.pem")?;
    assert_eq!((v.identity.clone(), v.to_string()), (Identity::new("hyper-light", Actor::Cd), "CD, for the kiln server of hyper-light".to_owned()));
    assert_eq!(v.acts_for(), vec![server_of.clone()]);
    // Their keys: each in its own 0600 file, nowhere else in the server's data.
    let kept = |k: &str| -> anyhow::Result<()> {
        assert_eq!(std::fs::metadata(pki.join(k))?.permissions().mode() & 0o777, 0o600, "{k}");
        let others: Vec<_> = walk(&data)?.into_iter().filter(|p| !p.ends_with(k)).collect();
        nowhere(&others, &read(k)?)
    };
    kept("server-signing.key")?;
    kept("cd-signing.key")?;
    // A restart keeps them: nothing new issued.
    s.restart()?;
    assert_eq!(identities(&s, "server")?.len(), 1);
    assert_eq!(identities(&s, "cd")?.len(), 1);
    // The server's certificate revoked: a new key and a certificate for it, at once, verifying as before.
    let (key, was) = (read("server-signing.key")?, kiln::pki::fingerprints(&read("server-signing.pem")?)?);
    revoke(&s, &serde_json::json!({ "serial": was.serial, "reason": "w27 step 7: a certificate revoked" }))?;
    let now = kiln::pki::fingerprints(&read("server-signing.pem")?)?;
    assert!(now.serial != was.serial && now.key != was.key && read("server-signing.key")? != key, "a new key: {was:?} then {now:?}");
    kept("server-signing.key")?;
    assert_eq!(verified(&s, &pki, "server-signing.pem")?.identity, server_of);
    // CD's key revoked: a new key, in its own 0600 file, and a certificate for it.
    let (key, was) = (read("cd-signing.key")?, kiln::pki::fingerprints(&read("cd-signing.pem")?)?);
    revoke(&s, &serde_json::json!({ "key": was.key, "reason": "w27 step 7: a key lost" }))?;
    let now = kiln::pki::fingerprints(&read("cd-signing.pem")?)?;
    assert!(now.key != was.key && read("cd-signing.key")? != key, "a new key: {was:?} then {now:?}");
    kept("cd-signing.key")?;
    assert_eq!(verified(&s, &pki, "cd-signing.pem")?.acts_for(), vec![server_of]);
    Ok(())
}

/// Every kind of actor on one server: the server and CD, its own runner (which runs a leg), a
/// runner beside it holding an agent session, and the owner certified from the CLI. The leg's
/// workflow is pushed to a repository of the server's own.
fn everything(dir: &Path) -> anyhow::Result<(Server, Vec<agents::Runner>)> {
    let s = start(dir, &Options { agent: true, ci: "kiln_workflows = true", extra: "" })?;
    let (runner, log) = agents::runner(dir, s.port, "box-a", None, TALK)?;
    agents::connected(s.port, &s.admin, "box-a", &log, Duration::from_secs(60))?;
    let cwd = dir.join("work-here");
    std::fs::create_dir_all(&cwd)?;
    agents::open_session(s.port, &s.admin, "box-a", &cwd, Duration::from_secs(60))?;
    let out = common::kiln()
        .args(["login", "--certificate", "--url", &format!("http://127.0.0.1:{}", s.port), "--token", &s.admin])
        .env("XDG_CONFIG_HOME", dir.join("owner-config"))
        .env("HOME", dir.join("home"))
        .output()?;
    anyhow::ensure!(out.status.success(), "kiln login --certificate: {}", String::from_utf8_lossy(&out.stderr));
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&serde_json::json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.join("githome"), dir.join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "an app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while http(s.port, "GET", "/api/repos", &s.admin, None)?.1.contains("\"last_polled_at\":null") {
        anyhow::ensure!(Instant::now() < deadline, "never polled");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::write(work.join(".kiln/e2e.yaml"), LEG)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "the workflow"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    for kind in ["server", "cd", "runner", "agent", "person", "leg"] {
        until_identities(&s, kind, Duration::from_secs(120), |l| l.iter().any(|i| !field(i, "verified").is_empty()))?;
    }
    Ok((s, vec![runner]))
}

/// W27-AC01: each kind of actor's certificate verifies to kiln's root and names whom it acts for;
/// a revoked identity's certificate is refused from the moment of revocation, saying so.
#[test]
fn w27_ac01_chains() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (s, _runners) = everything(dir.path())?;
    let want = [
        ("server", "kiln server of hyper-light"),
        ("cd", "CD, for the kiln server of hyper-light"),
        ("runner", ", enrolled by admin, of hyper-light"),
        ("agent", ", for admin, on runner box-a (instance main), of hyper-light"),
        ("person", "admin of hyper-light"),
        ("leg", ", enrolled by admin, of hyper-light"),
    ];
    for (kind, says) in want {
        let list = identities(&s, kind)?;
        let one = list.iter().find(|i| !field(i, "verified").is_empty()).cloned().unwrap_or_default();
        assert!(field(&one, "verified").ends_with(says) || field(&one, "verified") == says, "{kind}: {one}");
        // Revoked: refused from then, saying when, why and by whom.
        let (st, body) = http(s.port, "POST", "/api/identities/revoke", &s.admin, Some(&serde_json::json!({ "serial": field(&one, "serial"), "reason": format!("{kind} key lost") })))?;
        anyhow::ensure!(st == 200, "{kind}: {st} {body}");
        let after = identities(&s, kind)?;
        let gone = after.iter().find(|i| field(i, "serial") == field(&one, "serial")).cloned().unwrap_or_default();
        assert!(field(&gone, "refused").contains(&format!("{kind} key lost, by admin")), "{kind}: {gone}");
    }
    Ok(())
}

/// W27-AC02: no private key crosses the network. Every private key on disk is where it was made
/// and belongs: the CA's and the server's own in its data, each machine's in its state, the
/// person's in their config; none in the store, a log, or beside any identity's certificate.
#[test]
fn w27_ac02_keys() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (s, runners) = everything(dir.path())?;
    drop(runners);
    drop(s);
    let made_there = |p: &str| {
        p.ends_with("/data/pki/ca.key")
            || p.ends_with("/data/pki/server.key")
            || p.ends_with("-signing.key")
            || p.ends_with("/agent.key")
            || p.ends_with("/reach/identity.key")
            || p.ends_with("/owner-config/kiln/identity.key")
            || p.contains("/data/keys/")
    };
    let mut keys = Vec::new();
    for f in walk(dir.path())? {
        let name = f.to_string_lossy().into_owned();
        if name.contains("/githome/") || name.contains("/tree/") || name.contains("/work") {
            continue;
        }
        if pem_key(&std::fs::read(&f).unwrap_or_default()) {
            keys.push(name);
        }
    }
    let stray: Vec<&String> = keys.iter().filter(|k| !made_there(k)).collect();
    assert!(stray.is_empty(), "private keys away from where they were made: {stray:?} (all: {keys:?})");
    assert!(keys.iter().any(|k| k.ends_with("/owner-config/kiln/identity.key")), "the person's key is on their side: {keys:?}");
    Ok(())
}


/// A leg's workflow for the test runner alone, its steps as written (YAML, under `steps:`).
fn legs_steps_workflow(steps: &str) -> String {
    format!("on: push\njobs:\n  e2e:\n    runs-on: self-hosted\n    steps:\n{steps}")
}

/// The test run's leg: its id.
fn the_leg(dir: &Path) -> anyhow::Result<String> {
    let db = rusqlite::Connection::open(dir.join("data/kiln.db"))?;
    Ok(db.query_row("SELECT id FROM jobs WHERE key = 'e2e' ORDER BY rowid DESC LIMIT 1", [], |r| r.get(0))?)
}

/// The test runner's leg of run 1, as its certificate names it.
fn e2e_leg() -> kiln_ci::identity::chain::Identity {
    use kiln_ci::identity::chain::{Actor, Identity};
    Identity::new("hyper-light", Actor::Leg { enrolled_by: "admin".into(), machine: "fedora-2".into(), instance: "main".into(), run: 1, attempt: 1, leg: "e2e".into() })
}

/// W27-R05: a leg's logs, its steps' ends, its test report and its conclusion are signed as the
/// leg, on its runner, at its end. They verify so at the time kiln took them, and what kiln keeps
/// holds to them; a byte of its log changed afterwards is found.
#[test]
fn w27_r05_a_legs_results_verify_as_the_leg() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let steps = "      - run: echo building\n      - test-results: { path: report.xml }\n        run: |\n          printf '%s' '<?xml version=\"1.0\"?><testsuite name=\"calc\" tests=\"1\"><testcase classname=\"calc\" name=\"adds\"/></testsuite>' > report.xml\n          echo tested\n";
    let run = leg_on_a_test_runner(dir.path(), &legs_steps_workflow(steps), &[])?;
    let leg = the_leg(dir.path())?;
    let signed = || -> anyhow::Result<Value> {
        let (st, body) = http(run.s.port, "GET", &format!("/api/ci/legs/{leg}/signed"), &run.s.admin, None)?;
        anyhow::ensure!(st == 200, "{st} {body}\n{}", run.s.log());
        Ok(serde_json::from_str(&body)?)
    };
    let v = signed()?;
    assert_eq!(field(&v, "state"), "verified", "{v:#}");
    assert_eq!(field(&v, "uri"), "kiln://hyper-light/person/admin/machine/fedora-2/instance/main/run/1/attempt/1/leg/e2e");
    let short = run.sha.get(..7).unwrap_or_default();
    assert!(field(&v, "signer").starts_with(&format!("leg e2e of run 1 (attempt 1) at {short}, on runner fedora-2")), "{v:#}");
    let ends = v.get("steps").and_then(Value::as_array).cloned().unwrap_or_default();
    assert!(ends.len() >= 2 && ends.iter().all(|s| field(s, "status") == "succeeded"), "{v:#}");
    let reports = v.get("reports").and_then(Value::as_array).cloned().unwrap_or_default();
    assert_eq!(reports.iter().map(|r| field(r, "path")).collect::<Vec<_>>(), ["report.xml"], "{v:#}");
    let logs = v.get("logs").and_then(Value::as_array).cloned().unwrap_or_default();
    assert!(!logs.is_empty() && logs.iter().all(|l| l.pointer("/verdict/state").and_then(Value::as_str) == Some("verified")), "{v:#}");
    assert_eq!(v.pointer("/conclusion/status").and_then(Value::as_str), Some("succeeded"), "{v:#}");
    // Under the server's delegation of the leg, signed as the server, naming what it handed it.
    assert_eq!(v.pointer("/under/uri").and_then(Value::as_str), Some("kiln://hyper-light/server"), "{v:#}");
    let (delegation, delegated_at) = (v.pointer("/under/bundle").cloned().unwrap_or_default(), v.pointer("/under/taken_at").and_then(Value::as_i64).unwrap_or_default());
    let under = v.pointer("/under/digest").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no delegation: {v:#}"))?;
    // Each record verifies on its own, at the time kiln took it: the leg's as the leg on its runner
    // under that delegation, kiln's receipt as the server.
    let (root, revocations) = (root(&run.s)?, revocations_now(&run.s)?);
    let server = kiln_ci::identity::chain::Identity::new("hyper-light", kiln_ci::identity::chain::Actor::Server);
    let mut said = Vec::new();
    for r in v.get("records").and_then(Value::as_array).into_iter().flatten() {
        let (bundle, at) = (r.get("bundle").cloned().unwrap_or_default(), r.get("taken_at").and_then(Value::as_i64).unwrap_or_default());
        let u = kiln_ci::identity::statement::verify_under(&bundle, &root, &revocations, at, |d| (d == under).then(|| (delegation.clone(), delegated_at)))?;
        let by = &u.statement.signer.identity;
        assert!((*by == e2e_leg() && u.through.len() == 1) || (*by == server && u.through.is_empty()), "{by} under {} delegations", u.through.len());
        let first = u.statement.statement.subject.first().map(|s| s.name.clone()).unwrap_or_default();
        said.push((u.statement.payload_type.clone(), kiln_ci::attest::leg::kind(&u.statement).to_owned(), first, *by == server));
    }
    let has = |payload: &str, kind: &str, first: &str, kilns: bool| said.iter().any(|(p, k, f, s)| p == payload && k == kind && f.starts_with(first) && *s == kilns);
    let custody = kiln_ci::identity::statement::PAYLOAD_CUSTODY;
    assert!(has(kiln_ci::attest::leg::PAYLOAD_RESULTS, "", "log/", false), "its results: {said:?}");
    assert!(has(custody, "received", "assignment", false) && has(custody, "received", "checkout", false), "its receipts: {said:?}");
    assert!(has(custody, "handed-on", "log/", false) && has(custody, "received", "log/", true), "its hand-on, and kiln's receipt of it: {said:?}");
    // A byte of the log changed since: found.
    let shard = leg.get(leg.len().saturating_sub(2)..).unwrap_or("00");
    let logs_dir = dir.path().join("data/logs").join(shard).join(&leg);
    let built = std::fs::read_dir(&logs_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|x| x == "log") && std::fs::read(p).is_ok_and(|b| b.windows(8).any(|w| w == b"building")))
        .ok_or_else(|| anyhow::anyhow!("no log says building in {}", logs_dir.display()))?;
    let mut bytes = std::fs::read(&built)?;
    let at = bytes.windows(8).position(|w| w == b"building");
    if let Some(b) = at.and_then(|at| bytes.get_mut(at)) {
        *b = b'B';
    }
    std::fs::write(&built, &bytes)?;
    let v = signed()?;
    assert_eq!(field(&v, "state"), "broken", "{v:#}");
    let breaks = |v: &Value| -> Vec<String> { v.get("breaks").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect() };
    assert!(breaks(&v).iter().any(|b| b.contains("isn't what the leg sent")), "{:#?}", breaks(&v));
    // A log kiln keeps as the leg's, of a step the leg signed no log of: found too.
    let forged: &[u8] = b"forged\n";
    std::fs::write(logs_dir.join("7.log"), forged)?;
    let piece = kiln_ci::logs::Piece { at: Some(0), len: forged.len() as u64, digest: kiln_ci::logs::digest(forged) };
    std::fs::write(logs_dir.join("7.digests"), format!("{}\n", kiln_ci::logs::line(&piece)))?;
    let v = signed()?;
    assert!(breaks(&v).iter().any(|b| b == "step 7's log: kiln keeps 1 chunk of it as the leg's, and the leg signed none"), "{:#?}", breaks(&v));
    Ok(())
}

/// W27-R05: a v1 job's results (a `.kiln/` workflow on v1, as production runs them) are signed as
/// the leg on its runner, as a v2 leg's are: they verify under kiln's delegation of the job, which
/// names the commit it was given, its steps' ends and logs among them.
#[test]
fn w27_r05_a_v1_jobs_results_verify_as_the_leg() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let pipeline = "name: e2e\non:\n  push: { branches: [main] }\njobs:\n  e2e:\n    runs-on: [self-hosted]\n    steps:\n      - run: echo building\n      - run: echo tested\n";
    let run = v1_job_on_a_test_runner(dir.path(), pipeline)?;
    let leg = the_leg(dir.path())?;
    let (st, body) = http(run.s.port, "GET", &format!("/api/ci/legs/{leg}/signed"), &run.s.admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}\n{}", run.s.log());
    let v: Value = serde_json::from_str(&body)?;
    assert_eq!(field(&v, "state"), "verified", "{v:#}");
    let uri = field(&v, "uri");
    assert!(uri.starts_with("kiln://hyper-light/person/admin/machine/fedora-2/instance/main/run/") && uri.ends_with("/attempt/1/leg/e2e"), "{v:#}");
    let ends = v.get("steps").and_then(Value::as_array).cloned().unwrap_or_default();
    assert!(ends.len() >= 2 && ends.iter().all(|s| field(s, "status") == "succeeded"), "{v:#}");
    let logs = v.get("logs").and_then(Value::as_array).cloned().unwrap_or_default();
    assert!(!logs.is_empty() && logs.iter().all(|l| l.pointer("/verdict/state").and_then(Value::as_str) == Some("verified")), "{v:#}");
    assert_eq!(v.pointer("/under/uri").and_then(Value::as_str), Some("kiln://hyper-light/server"), "{v:#}");
    // kiln's delegation of the job names its commit.
    let delegation = v.pointer("/under/bundle/dsseEnvelope/payload").and_then(Value::as_str).unwrap_or_default();
    let said = String::from_utf8(base64::Engine::decode(&base64::engine::general_purpose::STANDARD, delegation)?)?;
    assert!(said.contains(&run.sha), "the delegation names the job's commit {}: {said}", run.sha);
    Ok(())
}

/// W27-R05: a runner behind its legs, as a busy machine's falls behind (a test build's `lag`),
/// still signs all a leg sent. A leg's end that comes while its last sends wait their turn (its test
/// step's report line, that step's end) goes after them: its results verify, that step's end among
/// them.
/// `cargo test -p kiln --features chaos --test identities w27_r05_a_runner_behind_its_legs_signs_all_they_sent`
#[cfg(feature = "chaos")]
#[test]
fn w27_r05_a_runner_behind_its_legs_signs_all_they_sent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let steps = "      - run: echo building\n      - test-results: { path: report.xml }\n        run: |\n          printf '%s' '<?xml version=\"1.0\"?><testsuite name=\"calc\" tests=\"1\"><testcase classname=\"calc\" name=\"adds\"/></testsuite>' > report.xml\n          echo tested\n";
    let run = leg_on_a_test_runner_with(dir.path(), &legs_steps_workflow(steps), &[], Some("lag=300ms"))?;
    let leg = the_leg(dir.path())?;
    let (st, body) = http(run.s.port, "GET", &format!("/api/ci/legs/{leg}/signed"), &run.s.admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}\n{}", run.s.log());
    let v: Value = serde_json::from_str(&body)?;
    assert_eq!(field(&v, "state"), "verified", "{v:#}");
    let ends: Vec<u64> = v.get("steps").and_then(Value::as_array).into_iter().flatten().filter_map(|s| s.get("idx").and_then(Value::as_u64)).collect();
    assert!(ends.contains(&0) && ends.contains(&1), "both steps' ends are signed: {v:#}");
    Ok(())
}

/// W27-R05: a leg on a runner from before legs signed (a test build's `old`) sends no signed
/// records. The API says the leg's are unsigned and why, never verified.
#[cfg(feature = "chaos")]
#[test]
fn w27_r05_a_leg_on_an_older_runner_reads_unsigned_with_why() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let run = leg_on_a_test_runner_with(dir.path(), &legs_steps_workflow("      - run: echo built\n"), &[], Some("old"))?;
    let leg = the_leg(dir.path())?;
    let (st, body) = http(run.s.port, "GET", &format!("/api/ci/legs/{leg}/signed"), &run.s.admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}\n{}", run.s.log());
    let v: Value = serde_json::from_str(&body)?;
    assert_eq!(field(&v, "state"), "unsigned", "{v:#}");
    assert_eq!(field(&v, "why"), "kiln gave it no identity: its runner fedora-2 asked for none (a kiln from before them)", "{v:#}");
    Ok(())
}

/// W27-R06: an artifact's provenance, written by kiln from its own records, is signed as the leg
/// that built it, on its runner. Kept beside kiln's own, it verifies as the leg over the very
/// statement kiln wrote, naming its runner.
#[test]
fn w27_r06_provenance_verifies_as_the_leg_that_built_it() -> anyhow::Result<()> {
    use base64::Engine as _;
    let dir = tempfile::tempdir()?;
    let steps = "      - run: printf 'the artifact\\n' > artifact.txt\n      - uses: kiln/attest@v1\n        with: { subject-path: artifact.txt }\n      - run: sleep 1\n";
    let run = leg_on_a_test_runner(dir.path(), &legs_steps_workflow(steps), &[])?;
    let leg = the_leg(dir.path())?;
    let db = rusqlite::Connection::open(dir.path().join("data/kiln.db"))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let (kilns, legs) = loop {
        let mut st = db.prepare("SELECT origin, bundle FROM ci_attestations WHERE leg_id = ?1 AND predicate_type = ?2 ORDER BY id")?;
        let rows: Vec<(String, String)> = st.query_map(rusqlite::params![leg, kiln_ci::attest::PROVENANCE], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
        let of = |origin: &str| rows.iter().find(|(o, _)| o == origin).map(|(_, b)| b.clone());
        if let (Some(k), Some(l)) = (of("kiln"), of("leg")) {
            break (k, l);
        }
        anyhow::ensure!(Instant::now() < deadline, "no provenance signed as the leg: {rows:?}\n{}", run.s.log());
        std::thread::sleep(Duration::from_millis(300));
    };
    let (kilns, legs): (Value, Value) = (serde_json::from_str(&kilns)?, serde_json::from_str(&legs)?);
    let payload = |b: &Value| b.pointer("/dsseEnvelope/payload").and_then(Value::as_str).map(str::to_owned);
    assert_eq!(payload(&legs), payload(&kilns), "the leg signs the very statement kiln wrote");
    let b64 = |p: &str| base64::engine::general_purpose::STANDARD.decode(legs.pointer(p).and_then(Value::as_str).unwrap_or_default());
    let (leaf, body, sig) = (b64("/verificationMaterial/certificate/rawBytes")?, b64("/dsseEnvelope/payload")?, b64("/dsseEnvelope/signatures/0/sig")?);
    let v = kiln_ci::identity::chain::verify_signed(&[&leaf], &root(&run.s)?, &revocations_now(&run.s)?, unix_now()?, &kiln_ci::attest::pae(kiln_ci::attest::PAYLOAD_TYPE, &body), &sig)?;
    assert_eq!(v.identity, e2e_leg());
    // Its words are still kiln's: SLSA provenance of the artifact the leg built, naming its runner.
    let st: Value = serde_json::from_slice(&body)?;
    assert_eq!(st.get("predicateType").and_then(Value::as_str), Some(kiln_ci::attest::PROVENANCE));
    assert!(st.pointer("/subject/0/name").and_then(Value::as_str).is_some_and(|n| n.ends_with("artifact.txt")), "{st}");
    assert_eq!(st.pointer("/predicate/runDetails/byproducts/0/annotations/runner").and_then(Value::as_str), Some("fedora-2"), "{st}");
    // Its one material is the commit the leg checked out, which is the one pushed.
    let materials = st.pointer("/predicate/buildDefinition/resolvedDependencies").and_then(Value::as_array).cloned().unwrap_or_default();
    assert_eq!(materials.iter().map(|m| m.pointer("/digest/gitCommit").and_then(Value::as_str).unwrap_or_default()).collect::<Vec<_>>(), [run.sha.as_str()], "{st}");
    let signed: Value = {
        let (code, body) = http(run.s.port, "GET", &format!("/api/ci/legs/{leg}/signed"), &run.s.admin, None)?;
        anyhow::ensure!(code == 200, "{code} {body}");
        serde_json::from_str(&body)?
    };
    let short = run.sha.get(..7).unwrap_or_default();
    assert!(field(&signed, "signer").starts_with(&format!("leg e2e of run 1 (attempt 1) at {short}, on runner fedora-2")), "{signed:#}");
    Ok(())
}

/// A personal access token for person `name`, as much as they may.
fn token_of(s: &Server, name: &str) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&serde_json::json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}'s token: {st} {body}");
    serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))
}

/// W27-AC04: a verdict verifies on its revision. Kim approves D1 on her laptop (`kiln review
/// --accept`, signed with `kiln login`'s key over v2's commit and verbatim patch-id). Her
/// approval holds on v5, after two rewords and a rebase onto a moved trunk (the same verbatim
/// patch-id), and not on v6, which changed the code, saying so. Once the trunk's policy requires
/// signatures, an unsigned verdict is refused, saying how to sign, and the gate counts only
/// verdicts that hold on the version it judges: Kim's signed one, not Bob's unsigned one.
#[test]
fn w27_ac04_a_verdict_verifies_on_its_revision() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&serde_json::json!({"name": "acme/review", "mode": "native"})))?;
    anyhow::ensure!(st == 200, "a repository kiln hosts: {st} {body}");
    for name in ["sam", "kim", "bob"] {
        add_person(&s, name)?;
    }
    let (sam, kim, bob) = (token_of(&s, "sam")?, token_of(&s, "kim")?, token_of(&s, "bob")?);
    let laptop = Device::new(dir.path(), "kim-laptop")?;
    let (ok, said) = laptop.kiln(&["login", "--link", &sign_in_link(&s, "kim")?], None)?;
    anyhow::ensure!(ok, "kiln login --link: {said}");
    let base = format!("http://127.0.0.1:{}", s.port);
    let review = |args: &[&str]| -> anyhow::Result<String> {
        let (ok, said) = laptop.kiln(&[&["review", "--url", base.as_str(), "--token", kim.as_str()][..], args].concat(), None)?;
        anyhow::ensure!(ok, "kiln review {args:?}: {said}");
        Ok(said)
    };
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/review.git", s.port);
    let git = |at: &Path, args: &[&str]| -> anyhow::Result<String> {
        let o = std::process::Command::new("git")
            .args(["-c", "user.name=sam", "-c", "user.email=sam@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
            .args(args)
            .current_dir(at)
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()?;
        let said = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        anyhow::ensure!(o.status.success(), "git {args:?}: {said}");
        Ok(said)
    };
    let verdicts = || -> anyhow::Result<Vec<Value>> {
        let (st, body) = http(s.port, "GET", "/api/diffs/D1/verdicts", &s.admin, None)?;
        anyhow::ensure!(st == 200, "verdicts: {st} {body}");
        Ok(serde_json::from_str::<Value>(&body)?.get("verdicts").and_then(Value::as_array).cloned().unwrap_or_default())
    };
    let kims = || -> anyhow::Result<Value> { verdicts()?.into_iter().rev().find(|v| field(v, "reviewer") == "user:kim").ok_or_else(|| anyhow::anyhow!("no verdict of kim's")) };

    git(dir.path(), &["clone", "-q", &url(&sam), "work"])?;
    let work = dir.path().join("work");
    std::fs::write(work.join("a.py"), "def f():\n    return 1\n")?;
    std::fs::write(work.join("z.txt"), "far away\n")?;
    git(&work, &["add", "."])?;
    git(&work, &["commit", "-qm", "first"])?;
    git(&work, &["push", "-q", "origin", "main"])?;
    git(&work, &["checkout", "-qb", "d1"])?;
    let message = "f returns two\n\nChange-Id: I4141414141414141414141414141414141414141\n";
    std::fs::write(work.join("a.py"), "def f():\n    return 2\n")?;
    git(&work, &["commit", "-qam", message])?;
    git(&work, &["push", "origin", "HEAD:refs/for/main"])?;
    std::fs::write(work.join("a.py"), "def f():\n    return 2  # two\n")?;
    git(&work, &["commit", "-q", "--amend", "-a", "-m", message])?;
    let said = git(&work, &["push", "origin", "HEAD:refs/for/main"])?;
    anyhow::ensure!(said.contains("D1 v2"), "v2: {said}");

    // Kim approves v2, signed on her laptop.
    review(&["D1", "--accept", "-m", "looks right"])?;
    let v = kims()?;
    assert_eq!((field(&v, "signer"), field(&v, "standing"), v.get("version")), ("kiln://hyper-light/person/kim", "given", Some(&serde_json::json!(2))), "{v}");
    // Two rewords and a rebase onto a moved trunk: the same verbatim patch-id, so it holds on v5.
    for (n, words) in [(3, "f returns 2"), (4, "f returns two, as asked")] {
        git(&work, &["commit", "-q", "--amend", "-m", &format!("{words}\n\nChange-Id: I4141414141414141414141414141414141414141\n")])?;
        let said = git(&work, &["push", "origin", "HEAD:refs/for/main"])?;
        anyhow::ensure!(said.contains(&format!("D1 v{n}")), "v{n}: {said}");
    }
    git(&work, &["checkout", "-q", "main"])?;
    std::fs::write(work.join("z.txt"), "far away, moved\n")?;
    git(&work, &["commit", "-qam", "the trunk moves"])?;
    git(&work, &["push", "-q", &url(&s.admin), "HEAD:main"])?;
    git(&work, &["checkout", "-q", "d1"])?;
    git(&work, &["rebase", "-q", "main"])?;
    let said = git(&work, &["push", "origin", "HEAD:refs/for/main"])?;
    anyhow::ensure!(said.contains("D1 v5"), "v5: {said}");
    let v = kims()?;
    assert_eq!((field(&v, "standing"), v.get("on")), ("carried", Some(&serde_json::json!(5))), "carried to v5: {v}");
    // v6 changes the code: her approval doesn't hold there, and says why.
    std::fs::write(work.join("a.py"), "def f():\n    return 3\n")?;
    git(&work, &["commit", "-q", "--amend", "-a", "-m", message])?;
    let said = git(&work, &["push", "origin", "HEAD:refs/for/main"])?;
    anyhow::ensure!(said.contains("D1 v6"), "v6: {said}");
    let v = kims()?;
    assert_eq!(field(&v, "standing"), "not", "on v6: {v}");
    assert!(field(&v, "why").contains("v6 changed the code"), "{v}");

    // Bob accepts v6 unsigned while the trunk only warns; then the trunk requires signatures.
    let (st, body) = http(s.port, "POST", "/api/diffs/D1/review", &bob, Some(&serde_json::json!({ "action": "accept" })))?;
    anyhow::ensure!(st == 200, "bob's unsigned accept, taken where signatures aren't required: {st} {body}");
    git(&work, &["checkout", "-q", "main"])?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".kiln/review.yaml"), "signatures:\n  mode: required\n")?;
    git(&work, &["add", ".kiln/review.yaml"])?;
    git(&work, &["commit", "-qm", "signatures required"])?;
    git(&work, &["push", "-q", &url(&s.admin), "HEAD:main"])?;
    let (st, body) = http(s.port, "POST", "/api/diffs/D1/review", &s.admin, Some(&serde_json::json!({ "action": "comment", "body": "unsigned" })))?;
    assert_eq!(st, 428, "an unsigned verdict where they're required: {body}");
    assert!(body.contains("requires signatures") && body.contains("/statement"), "says how to sign: {body}");
    review(&["D1", "--accept", "-m", "v6 too"])?;
    let (st, body) = http(s.port, "GET", "/api/diffs/D1/land", &s.admin, None)?;
    anyhow::ensure!(st == 200, "land: {st} {body}");
    let land: Value = serde_json::from_str(&body)?;
    assert_eq!(land.get("accepting"), Some(&serde_json::json!(["user:kim"])), "only the verdict that holds counts: {land}");

    // Comments too: each signed by whoever gives it, with the key on Kim's laptop, and its words
    // again when they're edited; one given by someone other than its signer is refused.
    let key = rcgen::KeyPair::from_pem(&std::fs::read_to_string(laptop.file("identity.key"))?)?;
    let cert = kiln::pki::der_of(&std::fs::read_to_string(laptop.file("identity.pem"))?)?;
    let sign = |statement: &str, ask: &Value| -> anyhow::Result<Value> {
        use base64::Engine as _;
        use rcgen::SigningKey as _;
        let (st, body) = http(s.port, "POST", statement, &kim, Some(ask))?;
        anyhow::ensure!(st == 200, "what to sign: {st} {body}");
        let t: Value = serde_json::from_str(&body)?;
        let b64 = base64::engine::general_purpose::STANDARD;
        let (payload, pae) = (b64.decode(field(&t, "payload"))?, b64.decode(field(&t, "pae"))?);
        Ok(kiln_ci::identity::statement::bundle(field(&t, "payload_type"), &payload, &key.sign(&pae)?, &cert))
    };
    let said = serde_json::json!({ "body": "why 3?", "file": "a.py", "start_line": 2 });
    let (st, body) = http(s.port, "POST", "/api/diffs/D1/comments", &kim, Some(&said))?;
    assert_eq!(st, 428, "an unsigned comment where they're required: {body}");
    let signed = sign("/api/diffs/D1/statement", &serde_json::json!({ "kind": "comment", "body": "why 3?", "file": "a.py", "start_line": 2 }))?;
    let (st, body) = http(s.port, "POST", "/api/diffs/D1/comments", &bob, Some(&serde_json::json!({ "body": "why 3?", "file": "a.py", "start_line": 2, "signed": signed })))?;
    assert_eq!(st, 403, "Kim's signature on Bob's comment: {body}");
    let (st, body) = http(s.port, "POST", "/api/diffs/D1/comments", &kim, Some(&serde_json::json!({ "body": "why 3?", "file": "a.py", "start_line": 2, "signed": signed })))?;
    assert_eq!(st, 200, "Kim's signed comment: {body}");
    let comment = field(&serde_json::from_str::<Value>(&body)?, "id").to_owned();
    let path = format!("/api/comments/{comment}");
    let (st, body) = http(s.port, "PATCH", &path, &kim, Some(&serde_json::json!({ "body": "why 3, not 2?" })))?;
    assert_eq!(st, 428, "new words unsigned: {body}");
    let other = sign(&format!("{path}/statement"), &serde_json::json!({ "body": "why 3, not 4?" }))?;
    let (st, body) = http(s.port, "PATCH", &path, &kim, Some(&serde_json::json!({ "body": "why 3, not 2?", "signed": other })))?;
    assert_eq!(st, 422, "signed for other words: {body}");
    let signed = sign(&format!("{path}/statement"), &serde_json::json!({ "body": "why 3, not 2?" }))?;
    let (st, body) = http(s.port, "PATCH", &path, &kim, Some(&serde_json::json!({ "body": "why 3, not 2?", "signed": signed })))?;
    assert_eq!(st, 200, "the new words, signed: {body}");
    Ok(())
}

/// W27-R04: an agent's verdict is signed as the agent, by the runner holding its key, never by the
/// server. The agent gives it as agents do, through its MCP server (`kiln reach mcp`'s `code_run`,
/// `kiln.ci.diffs.review`): that server asks kiln what to sign and has the runner sign it for the
/// session, as kiln-git-sign has its commits signed. The verdict kiln keeps verifies as the agent at
/// its epoch on its runner, for its person.
#[test]
fn w27_r04_an_agents_verdict_is_signed_by_its_runner() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&serde_json::json!({"name": "acme/agent-review", "mode": "native"})))?;
    anyhow::ensure!(st == 200, "a repository kiln hosts: {st} {body}");
    add_person(&s, "sam")?;
    let sam = token_of(&s, "sam")?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let git = |at: &Path, args: &[&str]| -> anyhow::Result<String> {
        let o = std::process::Command::new("git")
            .args(["-c", "user.name=sam", "-c", "user.email=sam@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
            .args(args)
            .current_dir(at)
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()?;
        let said = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        anyhow::ensure!(o.status.success(), "git {args:?}: {said}");
        Ok(said)
    };
    git(dir.path(), &["clone", "-q", &format!("http://x:{sam}@127.0.0.1:{}/git/acme/agent-review.git", s.port), "work"])?;
    let work = dir.path().join("work");
    std::fs::write(work.join("a.py"), "def f():\n    return 1\n")?;
    git(&work, &["add", "."])?;
    git(&work, &["commit", "-qm", "first"])?;
    git(&work, &["push", "-q", "origin", "main"])?;
    std::fs::write(work.join("a.py"), "def f():\n    return 2\n")?;
    git(&work, &["commit", "-qam", "f returns two\n\nChange-Id: I4242424242424242424242424242424242424242\n"])?;
    let said = git(&work, &["push", "origin", "HEAD:refs/for/main"])?;
    anyhow::ensure!(said.contains("D1 v1"), "D1: {said}");

    // What the agent does with the message `review <its session's id>`: accept D1 through its MCP
    // server (other messages, kiln's own first one, it leaves).
    let cwd = dir.path().join("agent-here");
    std::fs::create_dir_all(&cwd)?;
    let act = dir.path().join("act.sh");
    let call = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "code_run", "arguments": {"program": "return await kiln.ci.diffs.review({diff: \"D1\", action: \"accept\", body: \"from the agent\"})"}}});
    let init = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}});
    std::fs::write(
        &act,
        format!(
            "#!/bin/sh\ntext=$(cat)\ncase \"$text\" in review\\ *) KILN_SESSION=${{text#review }} ;; *) exit 0 ;; esac\nexport KILN_SESSION\n{{ printf '%s\\n' '{init}' '{call}'; while ! grep -q '\"id\":2' mcp.out 2>/dev/null; do sleep 0.2; done; }} | '{}' reach mcp > mcp.out 2> mcp.err\nprintf '\\n#act-ended\\n' >> mcp.out\n",
            env!("CARGO_BIN_EXE_kiln")
        ),
    )?;
    std::fs::set_permissions(&act, std::fs::Permissions::from_mode(0o755))?;
    let act_path = act.to_string_lossy().into_owned();
    let (_main, log) = agents::runner_with(dir.path(), s.port, "box-a", None, TALK, &[("FAKE_CLAUDE_ACT", &act_path)])?;
    agents::connected(s.port, &s.admin, "box-a", &log, Duration::from_secs(60))?;
    let id = agents::open_session(s.port, &s.admin, "box-a", &cwd, Duration::from_secs(60))?;
    let epoch = format!("/agent/{id}/epoch/");
    until_identities(&s, "agent", Duration::from_secs(60), |l| l.iter().any(|i| field(i, "subject").contains(&epoch) && !field(i, "verified").is_empty()))?;
    let (st, sent) = http(s.port, "POST", &format!("/api/harness/sessions/{id}/command"), &s.admin, Some(&serde_json::json!({"type": "send", "id": "review", "text": format!("review {id}")})))?;
    anyhow::ensure!((200..300).contains(&st), "send: {st} {sent}");
    let out = || std::fs::read_to_string(cwd.join("mcp.out")).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(60);
    // The act's own end, a line no answer is (kiln's MCP instructions say `done`, in `goals_done`).
    while !out().lines().any(|l| l == "#act-ended") {
        anyhow::ensure!(Instant::now() < deadline, "the agent's MCP server never answered: {}\n{}", out(), std::fs::read_to_string(cwd.join("mcp.err")).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(200));
    }
    let (st, body) = http(s.port, "GET", "/api/diffs/D1/verdicts", &s.admin, None)?;
    anyhow::ensure!(st == 200, "verdicts: {st} {body}");
    let verdicts: Value = serde_json::from_str(&body)?;
    let v = verdicts.get("verdicts").and_then(Value::as_array).and_then(|l| l.first()).cloned().ok_or_else(|| anyhow::anyhow!("no verdict: {body}\nthe agent's MCP server said: {}", out()))?;
    assert_eq!((field(&v, "action"), field(&v, "standing")), ("accept", "given"), "{v}");
    let signer = kiln_ci::identity::chain::Identity::parse(field(&v, "signer")).map_err(|e| anyhow::anyhow!("{e}: {v}"))?;
    assert!(matches!(&signer.actor, kiln_ci::identity::chain::Actor::Agent { agent, held_by: Some(h), person, .. } if *agent == id && h.machine == "box-a" && person == "admin"), "as the agent, on its runner, for its person: {signer}");
    Ok(())
}

/// W27-R07: a land is signed by its queue. D1, its `gate` check required, is approved by Kim (signed
/// on her laptop) and landed through main's queue, which runs `gate` on the candidate. The land's
/// statement verifies to kiln's root as kiln's server, names main's queue, the candidate that
/// landed (main's new head) as its subject, D1 at its version with Kim's verdict, and the `gate`
/// run with its legs' signed results.
#[test]
fn w27_r07_a_land_is_signed_by_its_queue() -> anyhow::Result<()> {
    use kiln_ci::identity::{landing, statement};
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&serde_json::json!({"name": "acme/landed", "mode": "native"})))?;
    anyhow::ensure!(st == 200, "a repository kiln hosts: {st} {body}");
    for name in ["sam", "kim"] {
        add_person(&s, name)?;
    }
    let (sam, kim) = (token_of(&s, "sam")?, token_of(&s, "kim")?);
    let laptop = Device::new(dir.path(), "kim-laptop")?;
    let (ok, said) = laptop.kiln(&["login", "--link", &sign_in_link(&s, "kim")?], None)?;
    anyhow::ensure!(ok, "kiln login --link: {said}");
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = format!("http://x:{sam}@127.0.0.1:{}/git/acme/landed.git", s.port);
    let git = |at: &Path, args: &[&str]| -> anyhow::Result<String> {
        let o = std::process::Command::new("git")
            .args(["-c", "user.name=sam", "-c", "user.email=sam@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
            .args(args)
            .current_dir(at)
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()?;
        let said = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        anyhow::ensure!(o.status.success(), "git {args:?}: {said}");
        Ok(said)
    };
    git(dir.path(), &["clone", "-q", &url, "work"])?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(work.join(".kiln/review.yaml"), "land:\n  approvals: 1\n  checks: [gate]\n")?;
    std::fs::write(work.join(".github/workflows/gate.yml"), "name: gate\non: [pull_request, merge_group]\njobs:\n  gate:\n    runs-on: local\n    steps:\n      - run: echo gated\n")?;
    std::fs::write(work.join("a.py"), "def f():\n    return 1\n")?;
    git(&work, &["add", "."])?;
    git(&work, &["commit", "-qm", "first"])?;
    git(&work, &["push", "-q", "origin", "main"])?;
    std::fs::write(work.join("a.py"), "def f():\n    return 2\n")?;
    git(&work, &["commit", "-qam", "f returns two\n\nChange-Id: I7777777777777777777777777777777777777777\n"])?;
    let said = git(&work, &["push", "origin", "HEAD:refs/for/main"])?;
    anyhow::ensure!(said.contains("D1 v1"), "D1: {said}");
    let (ok, said) = laptop.kiln(&["review", "--url", &format!("http://127.0.0.1:{}", s.port), "--token", &kim, "D1", "--accept"], None)?;
    anyhow::ensure!(ok, "kiln review: {said}");
    let get = |path: &str| -> anyhow::Result<Value> {
        let (st, body) = http(s.port, "GET", path, &s.admin, None)?;
        anyhow::ensure!(st == 200, "{path}: {st} {body}");
        Ok(serde_json::from_str(&body)?)
    };
    let deadline = Instant::now() + Duration::from_secs(240);
    while get("/api/diffs/D1/land")?.get("blockers") != Some(&serde_json::json!([])) {
        anyhow::ensure!(Instant::now() < deadline, "D1's gate: {}", get("/api/diffs/D1/land")?);
        std::thread::sleep(Duration::from_millis(500));
    }
    let (st, body) = http(s.port, "POST", "/api/diffs/D1/land", &s.admin, Some(&serde_json::json!({})))?;
    anyhow::ensure!(st == 200, "land: {st} {body}");
    while field(&get("/api/diffs/D1")?, "state") != "landed" {
        anyhow::ensure!(Instant::now() < deadline, "D1 never landed: {}", get("/api/repos/acme%2Flanded/queue")?);
        std::thread::sleep(Duration::from_millis(500));
    }
    let tip = git(&work, &["ls-remote", "origin", "refs/heads/main"])?.split_whitespace().next().unwrap_or_default().to_owned();
    let signed = get(&format!("/api/repos/acme%2Flanded/lands/{tip}"))?;
    let bundle = signed.get("bundle").cloned().unwrap_or_default();
    let checked = statement::verify(&bundle, &root(&s)?, &revocations_now(&s)?, signed.get("signed_at").and_then(Value::as_i64).unwrap_or_default())?;
    assert_eq!(checked.custodian, "kiln://hyper-light/server", "signed as kiln's server, whose queue it is");
    let land = landing::read(&checked.payload_type, &checked.statement).map_err(|e| anyhow::anyhow!(e))?;
    assert_eq!((land.queue(), land.to.as_str(), land.candidate.as_str()), ("acme/landed:main".to_owned(), tip.as_str(), tip.as_str()), "the trunk moved to the candidate tested: {land:?}");
    assert!(!land.rebased && land.from == land.base && field(&signed, "head") == tip, "from the candidate's base, never applied again: {land:?}");
    let kims = get("/api/diffs/D1/verdicts")?.get("verdicts").and_then(Value::as_array).and_then(|l| l.first()).map(|v| field(v, "digest").to_owned()).unwrap_or_default();
    let d1 = land.batch.first().ok_or_else(|| anyhow::anyhow!("an empty batch: {land:?}"))?;
    assert_eq!((d1.diff.as_str(), d1.version, &d1.verdicts), ("D1", 1, &vec![kims]), "{land:?}");
    let gate = land.checks.iter().find(|c| c.name == "gate").ok_or_else(|| anyhow::anyhow!("no gate run: {land:?}"))?;
    assert_eq!(gate.conclusion, "success", "{land:?}");
    assert!(!gate.results.is_empty(), "the gate's legs' signed results: {land:?}");
    Ok(())
}
