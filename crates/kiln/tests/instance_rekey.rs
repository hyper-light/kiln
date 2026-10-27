//! A runner process beside the machine's usual one (`--instance`, `docs/releases.md`) whose machine
//! enrolled again since, with a new key: its own certificate is for the old key, so it asks for a
//! new one with the machine's certificate and reaches the server, as the name the machine has now.
//! Before, it built no TLS configuration from the two ("tls config: invalid configuration") and
//! retried forever, and an upgrade that started it timed out waiting for it to report in.
//!
//! `cargo test -p kiln --test instance_rekey`

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::agents::{Runner, connected};
use common::{Options, start};

/// `kiln agent` of the server at `port` on the machine whose state is `state`, named `name`:
/// enrolling with `token` when it isn't, as runner process `instance` when given one. Its log.
fn agent(dir: &Path, port: u16, state: &Path, name: &str, token: Option<&str>, instance: Option<&str>, log: &str) -> anyhow::Result<(Runner, PathBuf)> {
    let home = dir.join("home");
    std::fs::create_dir_all(&home)?;
    let log_path = dir.join(log);
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kiln"));
    cmd.args(["agent", "--server", &format!("http://127.0.0.1:{port}"), "--name", name]);
    if let Some(t) = token {
        cmd.args(["--token", t]);
    }
    if let Some(id) = instance {
        cmd.args(["--instance", id]);
    }
    let child: Child = cmd
        .arg("--work-dir")
        .arg(dir.join(format!("{log}-work")))
        .arg("--state-dir")
        .arg(state)
        .env("HOME", &home)
        .env("NO_COLOR", "1")
        .env_remove("KILN_REACH_SOCK")
        .stdout(Stdio::from(out.try_clone()?))
        .stderr(Stdio::from(out))
        .spawn()?;
    Ok((Runner(child), log_path))
}

/// Waits until `log` says `what`.
fn says(log: &Path, what: &str, within: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + within;
    loop {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        if text.contains(what) {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "the log never said {what:?}:\n{text}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_runner_process_whose_machine_enrolled_again_gets_a_certificate_for_the_new_key() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();
    let s = start(dir, &Options::default())?;
    let join = std::fs::read_to_string(dir.join("data/join.token"))?.trim().to_owned();
    let state = dir.join("machine");
    let within = Duration::from_secs(60);

    // The machine enrolls as `box`, and a runner process of its own (an upgrade's) gets its
    // certificate.
    let (main, log) = agent(dir, s.port, &state, "box", Some(&join), None, "main.log")?;
    connected(s.port, &s.admin, "box", &log, within)?;
    drop(main);
    let (beside, log) = agent(dir, s.port, &state, "box", None, Some("i1"), "i1.log")?;
    says(&log, "certificate for this runner process issued", within)?;
    says(&log, "server welcomed agent", within)?;
    drop(beside);
    let own = state.join("instances/i1/agent.pem");
    let first = std::fs::read_to_string(&own)?;

    // The machine enrolls again (another name, a new key), as an older `kiln join` did.
    for f in ["agent.pem", "agent.key"] {
        std::fs::remove_file(state.join(f))?;
    }
    let (main, log) = agent(dir, s.port, &state, "box2", Some(&join), None, "main2.log")?;
    connected(s.port, &s.admin, "box2", &log, within)?;
    drop(main);
    let key = std::fs::read_to_string(state.join("agent.key"))?;
    assert!(!kiln::pki::cert_matches_key(&first, &key), "the machine has a new key");

    // The runner process starts again: its certificate is for the old key, so it gets a new one
    // and reaches the server (welcomed only under its certificate's name: the machine's now).
    let (_beside, log) = agent(dir, s.port, &state, "box", None, Some("i1"), "i1-again.log")?;
    says(&log, "for a key the machine no longer has", within)?;
    says(&log, "server welcomed agent", within)?;
    let renewed = std::fs::read_to_string(&own)?;
    assert!(kiln::pki::cert_matches_key(&renewed, &key), "its certificate is for the machine's key");
    assert!(!std::fs::read_to_string(&log)?.contains("tls config"), "it never tried the old certificate with the new key");
    Ok(())
}
